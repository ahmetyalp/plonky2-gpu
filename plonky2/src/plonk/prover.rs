use alloc::vec::Vec;
use alloc::{format, vec};
use core::mem::swap;

#[cfg(feature = "cuda")]
use std::ffi::c_void;

use std::fs::File;
use std::io::Write;
use std::mem::transmute;
use std::process::exit;
use std::time;

use anyhow::{ensure, Result};
use maybe_rayon::*;

use crate::field::extension::Extendable;
use crate::field::polynomial::{PolynomialCoeffs, PolynomialValues};
use crate::field::types::Field;
use crate::field::zero_poly_coset::ZeroPolyOnCoset;
use crate::fri::oracle::PolynomialBatch;
use crate::hash::hash_types::RichField;
use crate::iop::challenger::Challenger;
use crate::iop::generator::generate_partial_witness;
use crate::iop::witness::{MatrixWitness, PartialWitness, Witness};
use crate::plonk::circuit_data::{CommonCircuitData, ProverOnlyCircuitData};
use crate::plonk::config::{GenericConfig, Hasher};
use crate::plonk::plonk_common::PlonkOracle;
use crate::plonk::proof::{OpeningSet, Proof, ProofWithPublicInputs};
use crate::plonk::vanishing_poly::eval_vanishing_poly_base_batch;
use crate::plonk::vars::EvaluationVarsBaseBatch;
use crate::timed;
use crate::util::partial_products::{partial_products_and_z_gx, quotient_chunk_products};
use crate::util::timing::TimingTree;
use crate::util::{ceil_div_usize, log2_ceil, transpose};
use plonky2_util::log2_strict;

#[cfg(feature = "cuda")]
use crate::fri::oracle::CudaInnerContext;
#[cfg(feature = "cuda")]
use plonky2_cuda;
#[cfg(feature = "cuda")]
use plonky2_cuda::DataSlice;
#[cfg(feature = "cuda")]
use rustacuda::memory::DeviceSlice;
#[cfg(feature = "cuda")]
use rustacuda::prelude::CopyDestination;
#[cfg(feature = "cuda")]
use rustacuda::memory::AsyncCopyDestination;

pub fn prove<F: RichField + Extendable<D>, C: GenericConfig<D, F=F>, const D: usize>(
    prover_data: &ProverOnlyCircuitData<F, C, D>,
    common_data: &CommonCircuitData<F, D>,
    inputs: PartialWitness<F>,
    timing: &mut TimingTree,
) -> Result<ProofWithPublicInputs<F, C, D>> {
    let config = &common_data.config;
    let num_challenges = config.num_challenges;
    let quotient_degree = common_data.quotient_degree();
    let degree = common_data.degree();

    let partition_witness = timed!(
        timing,
        &format!("run {} generators", prover_data.generators.len()),
        generate_partial_witness(inputs, prover_data, common_data)
    );

    let public_inputs = partition_witness.get_targets(&prover_data.public_inputs);
    let public_inputs_hash = C::InnerHasher::hash_public_inputs(&public_inputs);

    let witness = timed!(
        timing,
        "compute full witness",
        partition_witness.full_witness()
    );

    let wires_values: Vec<PolynomialValues<F>> = timed!(
        timing,
        "compute wire polynomials",
        witness
            .wire_values
            .par_iter()
            .map(|column| PolynomialValues::new(column.clone()))
            .collect()
    );

    let wires_commitment = timed!(
        timing,
        "compute wires commitment",
        PolynomialBatch::from_values(
            wires_values,
            config.fri_config.rate_bits,
            config.zero_knowledge && PlonkOracle::WIRES.blinding,
            config.fri_config.cap_height,
            timing,
            // &prover_data.fft_root_table_deg,
            prover_data.fft_root_table.as_ref(),
        )
    );
    let mut challenger = Challenger::<F, C::Hasher>::new();

    // Observe the instance.
    challenger.observe_hash::<C::Hasher>(prover_data.circuit_digest);
    challenger.observe_hash::<C::InnerHasher>(public_inputs_hash);

    challenger.observe_cap(&wires_commitment.merkle_tree.cap);
    let betas = challenger.get_n_challenges(num_challenges);
    let gammas = challenger.get_n_challenges(num_challenges);

    assert!(
        common_data.quotient_degree_factor < common_data.config.num_routed_wires,
        "When the number of routed wires is smaller that the degree, we should change the logic to avoid computing partial products."
    );
    let mut partial_products_and_zs = timed!(
        timing,
        "compute partial products",
        all_wires_permutation_partial_products(&witness, &betas, &gammas, prover_data, common_data)
    );

    // Z is expected at the front of our batch; see `zs_range` and `partial_products_range`.
    let plonk_z_vecs = partial_products_and_zs
        .iter_mut()
        .map(|partial_products_and_z| partial_products_and_z.pop().unwrap())
        .collect();
    let zs_partial_products = [plonk_z_vecs, partial_products_and_zs.concat()].concat();

    let partial_products_and_zs_commitment = timed!(
        timing,
        "commit to partial products and Z's",
        PolynomialBatch::from_values(
            zs_partial_products,
            config.fri_config.rate_bits,
            config.zero_knowledge && PlonkOracle::ZS_PARTIAL_PRODUCTS.blinding,
            config.fri_config.cap_height,
            timing,
            prover_data.fft_root_table.as_ref(),
        )
    );

    challenger.observe_cap(&partial_products_and_zs_commitment.merkle_tree.cap);

    let alphas = challenger.get_n_challenges(num_challenges);

    let quotient_polys = timed!(
        timing,
        "compute quotient polys",
        compute_quotient_polys(
            common_data,
            prover_data,
            &public_inputs_hash,
            &wires_commitment,
            &partial_products_and_zs_commitment,
            &betas,
            &gammas,
            &alphas,
            timing,
        )
    );

    // Compute the quotient polynomials, aka `t` in the Plonk paper.
    let all_quotient_poly_chunks :Vec<PolynomialCoeffs<F>> = timed!(
        timing,
        "split up quotient polys",
        quotient_polys
            .into_par_iter()
            .flat_map(|mut quotient_poly| {
                quotient_poly.trim_to_len(quotient_degree).expect(
                    "Quotient has failed, the vanishing polynomial is not divisible by Z_H",
                );
                // Split quotient into degree-n chunks.
                quotient_poly.chunks(degree)
            })
            .collect()
    );

    let quotient_polys_commitment = timed!(
        timing,
        "commit to quotient polys",
        PolynomialBatch::from_coeffs(
            all_quotient_poly_chunks,
            config.fri_config.rate_bits,
            config.zero_knowledge && PlonkOracle::QUOTIENT.blinding,
            config.fri_config.cap_height,
            timing,
            prover_data.fft_root_table.as_ref(),
        )
    );

    challenger.observe_cap(&quotient_polys_commitment.merkle_tree.cap);

    let zeta = challenger.get_extension_challenge::<D>();
    // To avoid leaking witness data, we want to ensure that our opening locations, `zeta` and
    // `g * zeta`, are not in our subgroup `H`. It suffices to check `zeta` only, since
    // `(g * zeta)^n = zeta^n`, where `n` is the order of `g`.
    let g = F::Extension::primitive_root_of_unity(common_data.degree_bits());
    ensure!(
        zeta.exp_power_of_2(common_data.degree_bits()) != F::Extension::ONE,
        "Opening point is in the subgroup."
    );

    let openings = timed!(
        timing,
        "construct the opening set",
        OpeningSet::new(
            zeta,
            g,
            &prover_data.constants_sigmas_commitment,
            &wires_commitment,
            &partial_products_and_zs_commitment,
            &quotient_polys_commitment,
            common_data,
        )
    );
    challenger.observe_openings(&openings.to_fri_openings());

    let opening_proof = timed!(
        timing,
        "compute opening proofs",
        PolynomialBatch::prove_openings(
            &common_data.get_fri_instance(zeta),
            &[
                &prover_data.constants_sigmas_commitment,
                &wires_commitment,
                &partial_products_and_zs_commitment,
                &quotient_polys_commitment,
            ],
            &mut challenger,
            &common_data.fri_params,
            timing,
            &mut None,
        )
    );

    let proof = Proof {
        wires_cap: wires_commitment.merkle_tree.cap,
        plonk_zs_partial_products_cap: partial_products_and_zs_commitment.merkle_tree.cap,
        quotient_polys_cap: quotient_polys_commitment.merkle_tree.cap,
        openings,
        opening_proof,
    };
    Ok(ProofWithPublicInputs {
        proof,
        public_inputs,
    })
}

#[cfg(feature = "cuda")]
pub fn my_prove<F: RichField + Extendable<D>, C: GenericConfig<D, F=F>, const D: usize>(
    prover_data: &ProverOnlyCircuitData<F, C, D>,
    common_data: &CommonCircuitData<F, D>,
    inputs: PartialWitness<F>,
    timing: &mut TimingTree,
    ctx: &mut crate::fri::oracle::CudaInvContext<F, C, D>,
) -> Result<ProofWithPublicInputs<F, C, D>> {
    let config = &common_data.config;
    let num_challenges = config.num_challenges;
    let quotient_degree = common_data.quotient_degree();
    let degree = common_data.degree();

    let partition_witness = timed!(
        timing,
        &format!("run {} generators", prover_data.generators.len()),
        generate_partial_witness(inputs, prover_data, common_data)
    );

    let (public_inputs_hash, public_inputs) = timed!(
        timing,
        "get public_inputs_hash",
        {
            let public_inputs = partition_witness.get_targets(&prover_data.public_inputs);
            let public_inputs_hash = C::InnerHasher::hash_public_inputs(&public_inputs);
            (public_inputs_hash, public_inputs)
        });

    let mut witness = timed!(
        timing,
        "compute full witness",
        partition_witness.my_full_witness()
    );

    let wires_values = &witness.my_wire_values;
    // let wires_values: Vec<PolynomialValues<F>> = timed!(
    //     timing,
    //     "compute wire polynomials",
    //     witness
    //         .wire_values
    //         .par_iter()
    //         .map(|column| PolynomialValues::new(column.clone()))
    //         .collect()
    // );
    assert!(wires_values.len() % degree == 0);

    let wires_commitment = timed!(
        timing,
        "compute wires commitment",
        PolynomialBatch::from_values_with_gpu(
        // PolynomialBatch::from_values(
            wires_values,
            common_data.config.num_wires,
            degree,
            config.fri_config.rate_bits,
            config.zero_knowledge && PlonkOracle::WIRES.blinding,
            config.fri_config.cap_height,
            timing,
            prover_data.fft_root_table.as_ref(),
            &prover_data.fft_root_table_deg,
            ctx,
        )
    );
    let mut challenger = Challenger::<F, C::Hasher>::new();

    let (betas, gammas) = timed!(
        timing,
        "observe_hash for betas and gammas",
        {
            // Observe the instance.
            challenger.observe_hash::<C::Hasher>(prover_data.circuit_digest);
            challenger.observe_hash::<C::InnerHasher>(public_inputs_hash);

            challenger.observe_cap(&wires_commitment.merkle_tree.cap);
            let betas = challenger.get_n_challenges(num_challenges);
            let gammas = challenger.get_n_challenges(num_challenges);
            (betas, gammas)
        });

    assert!(
        common_data.quotient_degree_factor < common_data.config.num_routed_wires,
        "When the number of routed wires is smaller that the degree, we should change the logic to avoid computing partial products."
    );
    let mut partial_products_and_zs = timed!(
        timing,
        "compute partial products",
        all_wires_permutation_partial_products(&witness, &betas, &gammas, prover_data, common_data)
    );

    // let zs_partial_products = timed!(
    //     timing,
    //     "get zs_partial_products",
    //     {
    //         // Z is expected at the front of our batch; see `zs_range` and `partial_products_range`.
    //         let plonk_z_vecs = partial_products_and_zs
    //             .iter_mut()
    //             .map(|partial_products_and_z| partial_products_and_z.pop().unwrap())
    //             .collect();
    //         let zs_partial_products =
    //             [plonk_z_vecs, partial_products_and_zs.concat()].concat().into_iter().flat_map(|v| v.values).collect::<Vec<_>>();
    //         zs_partial_products
    //     });
    // let zs_partial_products = &zs_partial_products;
    //
    // unsafe {
    //     let v = zs_partial_products;
    //     let mut file = File::create("zs_partial_products-new.bin").unwrap();
    //     file.write_all(std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len()*8));
    // }
    // exit(0);

    // Z is expected at the front of our batch; see `zs_range` and `partial_products_range`.
    let plonk_z_vecs = partial_products_and_zs
        .iter_mut()
        .map(|partial_products_and_z| partial_products_and_z.pop().unwrap())
        .collect();
    let zs_partial_products = [plonk_z_vecs, partial_products_and_zs.concat()].concat();
    println!("zs_partial_products len:{}, itemLen:{}", zs_partial_products.len(), zs_partial_products[0].values.len());


    // unsafe {
    //     let v = zs_partial_products.iter().flat_map(|p|p.values.to_vec()).collect::<Vec<_>>();
    //     let mut file = File::create("zs_partial_products-old.bin").unwrap();
    //     file.write_all(std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len()*8));
    // }

    let zs_partial_products = &zs_partial_products.iter().flat_map(|p|p.values.to_vec()).collect::<Vec<_>>();
    let partial_products_and_zs_commitment = timed!(
        timing,
        "commit to partial products and Z's",
        // PolynomialBatch::from_values(
        PolynomialBatch::from_values_with_gpu(
            zs_partial_products,
            zs_partial_products.len()/degree,
            degree,
            config.fri_config.rate_bits,
            config.zero_knowledge && PlonkOracle::ZS_PARTIAL_PRODUCTS.blinding,
            config.fri_config.cap_height,
            timing,
            prover_data.fft_root_table.as_ref(),
            &prover_data.fft_root_table_deg,
            ctx,
        )
    );


    // let partial_products_and_zs_commitment = timed!(
    //     timing,
    //     "commit to partial products and Z's",
    //     PolynomialBatch::from_values(
    //     // PolynomialBatch::from_values_with_gpu(
    //         zs_partial_products,
    //         // zs_partial_products.len()/degree,
    //         // degree,
    //         config.fri_config.rate_bits,
    //         config.zero_knowledge && PlonkOracle::ZS_PARTIAL_PRODUCTS.blinding,
    //         config.fri_config.cap_height,
    //         timing,
    //         prover_data.fft_root_table.as_ref(),
    //         // &prover_data.fft_root_table_deg,
    //         // ctx,
    //     )
    // );

    let alphas = timed!(
        timing,
        "observe_cap for alphas",
        {
            challenger.observe_cap(&partial_products_and_zs_commitment.merkle_tree.cap);

            let alphas = challenger.get_n_challenges(num_challenges);
            alphas
        });

    // let quotient_polys = timed!(
    //     timing,
    //     "compute quotient polys",
    //     compute_quotient_polys(
    //         common_data,
    //         prover_data,
    //         &public_inputs_hash,
    //         &wires_commitment,
    //         &partial_products_and_zs_commitment,
    //         &betas,
    //         &gammas,
    //         &alphas,
    //         timing,
    //     )
    // );

    timed!(
        timing,
        "compute quotient polys",
        {
            let poly_num = common_data.config.num_wires;
            let values_num_per_poly = degree;
            let lg_n = log2_strict(values_num_per_poly );
            let values_flatten_len = poly_num*values_num_per_poly;

            let rate_bits = config.fri_config.rate_bits;
            let blinding = config.zero_knowledge && PlonkOracle::WIRES.blinding;
            let salt_size = if blinding { 4 } else { 0 };

            let ext_values_flatten_len = (values_flatten_len+salt_size*values_num_per_poly) * (1<<rate_bits);
            let pad_extvalues_len = ext_values_flatten_len;
            let values_num_per_extpoly = values_num_per_poly*(1<<rate_bits);

            let (ext_values_device, remained) = ctx.cache_mem_device.split_at_mut(ctx.second_stage_offset);
            // let (_, ext_values_device) = front_msm.split_at(values_flatten_len);
            let root_table_device2 = &mut ctx.root_table_device2;
            let shift_inv_powers_device = &mut ctx.shift_inv_powers_device;


            let (partial_products_and_zs_commitment_leaves_device, alphas_device, betas_device, gammas_device,
                d_outs, d_quotient_polys) = timed!(
                timing,
                "copy params",
                {
                    let mut useCnt = 0;
                    // let partial_products_and_zs_commitment_leaves = if partial_products_and_zs_commitment.merkle_tree.my_leaves.is_empty() {
                    //     partial_products_and_zs_commitment.merkle_tree.leaves.concat()
                    // } else {
                    //     partial_products_and_zs_commitment.merkle_tree.my_leaves.to_vec()
                    // };
                    // // unsafe
                    // // {
                    // //     let mut file = File::create("partial_products_and_zs_commitment_leaves.bin").unwrap();
                    // //     file.write_all(std::slice::from_raw_parts(partial_products_and_zs_commitment_leaves.as_ptr() as *const u8, partial_products_and_zs_commitment_leaves.len()*8));
                    // // }
                    //
                    // useCnt = partial_products_and_zs_commitment_leaves.len();

                    // let (_, remained) = remained.split_at_mut(ctx.values_flatten2.len());

                    useCnt = zs_partial_products.len() << rate_bits;
                    let (data, remained) = remained.split_at_mut(useCnt);

                    let partial_products_and_zs_commitment_leaves_device =
                        DataSlice{ptr: data.as_ptr() as *const c_void, len: useCnt as i32 };
                    // unsafe {
                    //     transmute::<&mut DeviceSlice<F>, &mut DeviceSlice<u64>>(data).async_copy_from(
                    //         transmute::<&Vec<F>, &Vec<u64>>(&partial_products_and_zs_commitment_leaves),
                    //         &ctx.inner.stream
                    //     ).unwrap();
                    // }

                    useCnt = values_num_per_extpoly*2;
                    let (d_quotient_polys, remained) = remained.split_at_mut(useCnt);

                    useCnt = values_num_per_extpoly*2;
                    let (d_outs, remained) = remained.split_at_mut(useCnt);

                    useCnt = num_challenges;
                    let (d_alphas, remained) = remained.split_at_mut(useCnt);
                    unsafe {
                        transmute::<&mut DeviceSlice<F>, &mut DeviceSlice<u64>>(d_alphas).async_copy_from(
                            transmute::<&Vec<F>, &Vec<u64>>(&alphas),
                            &ctx.inner.stream
                        ).unwrap();
                    }
                    let alphas_device = DataSlice{ptr: d_alphas.as_ptr() as *const c_void, len: alphas.len() as i32 };

                    let (d_betas, remained) = remained.split_at_mut(useCnt);
                    unsafe {
                        transmute::<&mut DeviceSlice<F>, &mut DeviceSlice<u64>>(d_betas).async_copy_from(
                            transmute::<&Vec<F>, &Vec<u64>>(&betas),
                            &ctx.inner.stream
                        ).unwrap();
                    }
                    let betas_device = DataSlice{ptr: d_betas.as_ptr() as *const c_void, len: betas.len() as i32 };

                    let (d_gammas, remained) = remained.split_at_mut(useCnt);
                    unsafe {
                        transmute::<&mut DeviceSlice<F>, &mut DeviceSlice<u64>>(d_gammas).async_copy_from(
                            transmute::<&Vec<F>, &Vec<u64>>(&gammas),
                            &ctx.inner.stream
                        ).unwrap();
                    }
                    let gammas_device = DataSlice{ptr: d_gammas.as_ptr() as *const c_void, len: gammas.len() as i32 };

                    ctx.inner.stream.synchronize().unwrap();

                    (partial_products_and_zs_commitment_leaves_device, alphas_device, betas_device, gammas_device, d_outs, d_quotient_polys)
                }
            );

            let points_device = DataSlice{ptr: ctx.points_device.as_ptr() as *const c_void, len: ctx.points_device.len() as i32 };
            let z_h_on_coset_evals_device = DataSlice{ptr: ctx.z_h_on_coset_evals_device.as_ptr() as *const c_void, len: ctx.z_h_on_coset_evals_device.len() as i32 };
            let z_h_on_coset_inverses_device = DataSlice{ptr: ctx.z_h_on_coset_inverses_device.as_ptr() as *const c_void, len: ctx.z_h_on_coset_inverses_device.len() as i32 };
            let k_is_device = DataSlice{ptr: ctx.k_is_device.as_ptr() as *const c_void, len: ctx.k_is_device.len() as i32 };

            let constants_sigmas_commitment_leaves_device = DataSlice{
                ptr: ctx.constants_sigmas_commitment_leaves_device.as_ptr() as *const c_void,
                len: ctx.constants_sigmas_commitment_leaves_device.len() as i32,
            };
            let ctx_ptr :*mut CudaInnerContext = &mut ctx.inner;
            timed!(
                timing,
                "compute quotient polys with GPU",
                unsafe {
                    plonky2_cuda::compute_quotient_polys(
                        ext_values_device.as_ptr() as *const u64,

                        poly_num as i32,
                        values_num_per_poly as i32,
                        lg_n as i32,
                        root_table_device2.as_ptr() as *const u64,
                        shift_inv_powers_device.as_ptr() as *const u64,
                        rate_bits as i32,
                        salt_size as i32,

                        &partial_products_and_zs_commitment_leaves_device,
                        &constants_sigmas_commitment_leaves_device,

                        d_outs.as_mut_ptr() as *mut c_void,
                        d_quotient_polys.as_mut_ptr() as *mut c_void,

                        &points_device,
                        &z_h_on_coset_evals_device,
                        &z_h_on_coset_inverses_device,
                        &k_is_device,

                        &alphas_device,
                        &betas_device,
                        &gammas_device,

                        ctx_ptr as *mut core::ffi::c_void,
                    )
                }
            );
            // let mut quotient_polys_flatten :Vec<F> = vec![F::ZERO; values_num_per_extpoly*2];
            // timed!(
            //         timing,
            //         "copy result",
            //         {
            //             unsafe {
            //                 transmute::<&DeviceSlice<F>, &DeviceSlice<u64>>(d_quotient_polys).async_copy_to(
            //                 transmute::<&mut Vec<F>, &mut Vec<u64>>(&mut quotient_polys_flatten),
            //                 &ctx.inner.stream).unwrap();
            //                 ctx.inner.stream.synchronize().unwrap();
            //             }
            //         }
            //     );
            //
            // (quotient_polys_flatten.chunks(values_num_per_extpoly).map(|c|PolynomialCoeffs{coeffs: c.to_vec()}).collect::<Vec<_>>(), d_quotient_polys)
        });

    // // Compute the quotient polynomials, aka `t` in the Plonk paper.
    // let all_quotient_poly_chunks :Vec<PolynomialCoeffs<F>> = timed!(
    //     timing,
    //     "split up quotient polys",
    //     quotient_polys
    //         .into_par_iter()
    //         .flat_map(|mut quotient_poly| {
    //             quotient_poly.trim_to_len(quotient_degree).expect(
    //                 "Quotient has failed, the vanishing polynomial is not divisible by Z_H",
    //             );
    //             // Split quotient into degree-n chunks.
    //             quotient_poly.chunks(degree)
    //         })
    //         .collect()
    // );
    // println!("all_quotient_poly_chunks len:{}, itemLen:{}", all_quotient_poly_chunks.len(), all_quotient_poly_chunks[0].coeffs.len());

    assert!(quotient_degree == (degree << config.fri_config.rate_bits));
    // let quotient_polys_commitment = timed!(
    //     timing,
    //     "commit to quotient polys",
    //     PolynomialBatch::from_coeffs(
    //         all_quotient_poly_chunks,
    //         config.fri_config.rate_bits,
    //         config.zero_knowledge && PlonkOracle::QUOTIENT.blinding,
    //         config.fri_config.cap_height,
    //         timing,
    //         prover_data.fft_root_table.as_ref(),
    //     )
    // );

    println!("offset: {}, values: {}, zs product: {}",
             ctx.second_stage_offset, ctx.values_flatten2.len(), zs_partial_products.len()<<config.fri_config.rate_bits);
    let quotient_polys_commitment = timed!(
        timing,
        "commit to quotient polys",
        PolynomialBatch::from_coeffs_with_gpu(
            ctx.second_stage_offset+(zs_partial_products.len()<<config.fri_config.rate_bits),
            degree,
            num_challenges*(1 << config.fri_config.rate_bits),
            config.fri_config.rate_bits,
            config.zero_knowledge && PlonkOracle::QUOTIENT.blinding,
            config.fri_config.cap_height,
            timing,
            ctx,
        )
    );

    let (zeta, g) = timed!(
        timing,
        "get zeta and g",
        {
            challenger.observe_cap(&quotient_polys_commitment.merkle_tree.cap);

            let zeta = challenger.get_extension_challenge::<D>();
            // To avoid leaking witness data, we want to ensure that our opening locations, `zeta` and
            // `g * zeta`, are not in our subgroup `H`. It suffices to check `zeta` only, since
            // `(g * zeta)^n = zeta^n`, where `n` is the order of `g`.
            let g = F::Extension::primitive_root_of_unity(common_data.degree_bits());
            ensure!(
                zeta.exp_power_of_2(common_data.degree_bits()) != F::Extension::ONE,
                "Opening point is in the subgroup."
            );
                    (zeta, g)
        });
    let openings = timed!(
        timing,
        "construct the opening set",
        OpeningSet::new(
            zeta,
            g,
            &prover_data.constants_sigmas_commitment,
            &wires_commitment,
            &partial_products_and_zs_commitment,
            &quotient_polys_commitment,
            common_data,
        )
    );

    timed!(
        timing,
        "observe_openings",
            challenger.observe_openings(&openings.to_fri_openings())
        );

    let opening_proof = timed!(
        timing,
        "compute opening proofs",
        PolynomialBatch::prove_openings(
            &common_data.get_fri_instance(zeta),
            &[
                &prover_data.constants_sigmas_commitment,
                &wires_commitment,
                &partial_products_and_zs_commitment,
                &quotient_polys_commitment,
            ],
            &mut challenger,
            &common_data.fri_params,
            timing,
            &mut Some(ctx),
        )
    );

    let proof = Proof {
        wires_cap: wires_commitment.merkle_tree.cap,
        plonk_zs_partial_products_cap: partial_products_and_zs_commitment.merkle_tree.cap,
        quotient_polys_cap: quotient_polys_commitment.merkle_tree.cap,
        openings,
        opening_proof,
    };
    Ok(ProofWithPublicInputs {
        proof,
        public_inputs,
    })
}
/// Compute the partial products used in the `Z` polynomials.
fn all_wires_permutation_partial_products<
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F=F>,
    const D: usize,
>(
    witness: &MatrixWitness<F>,
    betas: &[F],
    gammas: &[F],
    prover_data: &ProverOnlyCircuitData<F, C, D>,
    common_data: &CommonCircuitData<F, D>,
) -> Vec<Vec<PolynomialValues<F>>> {
    (0..common_data.config.num_challenges)
        .map(|i| {
            wires_permutation_partial_products_and_zs(
                witness,
                betas[i],
                gammas[i],
                prover_data,
                common_data,
            )
        })
        .collect()
}

/// Compute the partial products used in the `Z` polynomial.
/// Returns the polynomials interpolating `partial_products(f / g)`
/// where `f, g` are the products in the definition of `Z`: `Z(g^i) = f / g`.
fn wires_permutation_partial_products_and_zs<
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F=F>,
    const D: usize,
>(
    witness: &MatrixWitness<F>,
    beta: F,
    gamma: F,
    prover_data: &ProverOnlyCircuitData<F, C, D>,
    common_data: &CommonCircuitData<F, D>,
) -> Vec<PolynomialValues<F>> {
    let degree = common_data.quotient_degree_factor;
    let subgroup = &prover_data.subgroup;
    let k_is = &common_data.k_is;
    let num_prods = common_data.num_partial_products;
    let all_quotient_chunk_products = subgroup
        .par_iter()
        .enumerate()
        .map(|(i, &x)| {
            let s_sigmas = &prover_data.sigmas[i];
            let numerators = (0..common_data.config.num_routed_wires).map(|j| {
                let wire_value = witness.get_wire(i, j);
                let k_i = k_is[j];
                let s_id = k_i * x;
                wire_value + beta * s_id + gamma
            });
            let denominators = (0..common_data.config.num_routed_wires)
                .map(|j| {
                    let wire_value = witness.get_wire(i, j);
                    let s_sigma = s_sigmas[j];
                    wire_value + beta * s_sigma + gamma
                })
                .collect::<Vec<_>>();
            let denominator_invs = F::batch_multiplicative_inverse(&denominators);
            let quotient_values = numerators
                .zip(denominator_invs)
                .map(|(num, den_inv)| num * den_inv)
                .collect::<Vec<_>>();

            quotient_chunk_products(&quotient_values, degree)
        })
        .collect::<Vec<_>>();

    let mut z_x = F::ONE;
    let mut all_partial_products_and_zs = Vec::new();
    for quotient_chunk_products in all_quotient_chunk_products {
        let mut partial_products_and_z_gx =
            partial_products_and_z_gx(z_x, &quotient_chunk_products);
        // The last term is Z(gx), but we replace it with Z(x), otherwise Z would end up shifted.
        swap(&mut z_x, &mut partial_products_and_z_gx[num_prods]);
        all_partial_products_and_zs.push(partial_products_and_z_gx);
    }

    transpose(&all_partial_products_and_zs)
        .into_par_iter()
        .map(PolynomialValues::new)
        .collect()
}

const BATCH_SIZE: usize = 32;

fn compute_quotient_polys<
    'a,
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F=F>,
    const D: usize,
>(
    common_data: &CommonCircuitData<F, D>,
    prover_data: &'a ProverOnlyCircuitData<F, C, D>,
    public_inputs_hash: &<<C as GenericConfig<D>>::InnerHasher as Hasher<F>>::Hash,
    wires_commitment: &'a PolynomialBatch<F, C, D>,
    zs_partial_products_commitment: &'a PolynomialBatch<F, C, D>,
    betas: &[F],
    gammas: &[F],
    alphas: &[F],
    timing: &mut TimingTree,
) -> Vec<PolynomialCoeffs<F>> {
    let num_challenges = common_data.config.num_challenges;
    let quotient_degree_bits = log2_ceil(common_data.quotient_degree_factor);
    assert!(
        quotient_degree_bits <= common_data.config.fri_config.rate_bits,
        "Having constraints of degree higher than the rate is not supported yet. \
        If we need this in the future, we can precompute the larger LDE before computing the `PolynomialBatch`s."
    );

    // We reuse the LDE computed in `PolynomialBatch` and extract every `step` points to get
    // an LDE matching `max_filtered_constraint_degree`.
    let step = 1 << (common_data.config.fri_config.rate_bits - quotient_degree_bits);
    // When opening the `Z`s polys at the "next" point in Plonk, need to look at the point `next_step`
    // steps away since we work on an LDE of degree `max_filtered_constraint_degree`.
    let next_step = 1 << quotient_degree_bits;

    let points = F::two_adic_subgroup(common_data.degree_bits() + quotient_degree_bits);
    let lde_size = points.len();

    let z_h_on_coset = ZeroPolyOnCoset::new(common_data.degree_bits(), quotient_degree_bits);
    println!("z_h_on_coset, n: {}, rate: {}", z_h_on_coset.n, z_h_on_coset.rate);
    println!("step: {}, next_step: {}, lde_size: {}", step, next_step, lde_size);
    println!("public_inputs_hash: {:?}", public_inputs_hash);
    unsafe {
    //     let mut file = File::create("zs_partial_products_commitment.polynomials.bin").unwrap();
    //     for value in zs_partial_products_commitment.polynomials.iter().flat_map(|v| v.coeffs.clone()) {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(&value)).unwrap();
    //     }
    //     let mut file = File::create("zs_partial_products_commitment.leaves.bin").unwrap();
    //     for value in zs_partial_products_commitment.merkle_tree.leaves.concat() {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(&value)).unwrap();
    //     }
    //     let mut file = File::create("zs_partial_products_commitment.digests.bin").unwrap();
    //     for value in zs_partial_products_commitment.merkle_tree.digests.iter() {
    //         file.write_all(std::mem::transmute::<&_, &[u8; 32]>(value)).unwrap();
    //     }
    //     let mut file = File::create("zs_partial_products_commitment.caps.bin").unwrap();
    //     for value in zs_partial_products_commitment.merkle_tree.cap.0.iter() {
    //         file.write_all(std::mem::transmute::<&_, &[u8; 32]>(value)).unwrap();
    //     }
    //
    //     let mut file = File::create("alphas.bin").unwrap();
    //     for value in alphas {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(value)).unwrap();
    //     }
    //
    //     let mut file = File::create("betas.bin").unwrap();
    //     for value in betas {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(value)).unwrap();
    //     }
    //     let mut file = File::create("gammas.bin").unwrap();
    //     for value in gammas {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(value)).unwrap();
    //     }
    //     let mut file = File::create("k_is.bin").unwrap();
    //     for value in common_data.k_is.iter() {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(value)).unwrap();
    //     }
    //
    //     let mut file = File::create("points.bin").unwrap();
    //     for value in points.iter() {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(value)).unwrap();
    //     }
    //
    //     let mut file = File::create("z_h_on_coset.evals.bin").unwrap();
    //     println!("z_h_on_coset.evals len: {}", z_h_on_coset.evals.len());
    //     for value in z_h_on_coset.evals.iter() {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(value)).unwrap();
    //     }
    //     let mut file = File::create("z_h_on_coset.inverses.bin").unwrap();
    //     for value in z_h_on_coset.inverses.iter() {
    //         file.write_all(std::mem::transmute::<&F, &[u8; 8]>(value)).unwrap();
    //     }
    }

    println!("alphas: {:?}", alphas);
    println!("betas: {:?}", betas);
    println!("gammas: {:?}", gammas);

    let points_batches = points.par_chunks(BATCH_SIZE);
    let num_batches = ceil_div_usize(points.len(), BATCH_SIZE);
    let quotient_values: Vec<Vec<F>> =  timed!(
        timing,
        "compute quotient values",
        points_batches
        .enumerate()
        .flat_map(|(batch_i, xs_batch)| {
            // Each batch must be the same size, except the last one, which may be smaller.
            debug_assert!(
                xs_batch.len() == BATCH_SIZE
                    || (batch_i == num_batches - 1 && xs_batch.len() <= BATCH_SIZE)
            );

            let indices_batch: Vec<usize> =
                (BATCH_SIZE * batch_i..BATCH_SIZE * batch_i + xs_batch.len()).collect();

            let mut shifted_xs_batch = Vec::with_capacity(xs_batch.len());
            let mut local_zs_batch = Vec::with_capacity(xs_batch.len());
            let mut next_zs_batch = Vec::with_capacity(xs_batch.len());
            let mut partial_products_batch = Vec::with_capacity(xs_batch.len());
            let mut s_sigmas_batch = Vec::with_capacity(xs_batch.len());

            let mut local_constants_batch_refs = Vec::with_capacity(xs_batch.len());
            let mut local_wires_batch_refs = Vec::with_capacity(xs_batch.len());

            for (&i, &x) in indices_batch.iter().zip(xs_batch) {
                let shifted_x = F::coset_shift() * x;
                let i_next = (i + next_step) % lde_size;
                let local_constants_sigmas = prover_data
                    .constants_sigmas_commitment
                    .get_lde_values(i, step);
                let local_constants = &local_constants_sigmas[common_data.constants_range()];
                let s_sigmas = &local_constants_sigmas[common_data.sigmas_range()];
                let local_wires = wires_commitment.get_lde_values(i, step);
                let local_zs_partial_products =
                    zs_partial_products_commitment.get_lde_values(i, step);
                let local_zs = &local_zs_partial_products[common_data.zs_range()];
                let next_zs = &zs_partial_products_commitment.get_lde_values(i_next, step)
                    [common_data.zs_range()];
                let partial_products =
                    &local_zs_partial_products[common_data.partial_products_range()];

                if i == 1048576 {
                    println!("i: {}, len: {}, lcs: {:?}", i, local_constants_sigmas.len(), local_constants_sigmas);
                    println!("i: {}, len: {}, lw: {:?}", i, local_wires.len(), local_wires);
                    println!("i: {}, len: {}, lzpp: {:?}", i, local_zs_partial_products.len(), local_zs_partial_products);
                    println!("i: {}, len: {}, nzs: {:?}", i, next_zs.len(), next_zs);
                }
                debug_assert_eq!(local_wires.len(), common_data.config.num_wires);
                debug_assert_eq!(local_zs.len(), num_challenges);

                local_constants_batch_refs.push(local_constants);
                local_wires_batch_refs.push(local_wires);

                shifted_xs_batch.push(shifted_x);
                local_zs_batch.push(local_zs);
                next_zs_batch.push(next_zs);
                partial_products_batch.push(partial_products);
                s_sigmas_batch.push(s_sigmas);
            }

            // NB (JN): I'm not sure how (in)efficient the below is. It needs measuring.
            let mut local_constants_batch =
                vec![F::ZERO; xs_batch.len() * local_constants_batch_refs[0].len()];
            for i in 0..local_constants_batch_refs[0].len() {
                for (j, constants) in local_constants_batch_refs.iter().enumerate() {
                    local_constants_batch[i * xs_batch.len() + j] = constants[i];
                }
            }

            let mut local_wires_batch =
                vec![F::ZERO; xs_batch.len() * local_wires_batch_refs[0].len()];
            for i in 0..local_wires_batch_refs[0].len() {
                for (j, wires) in local_wires_batch_refs.iter().enumerate() {
                    local_wires_batch[i * xs_batch.len() + j] = wires[i];
                }
            }

            let vars_batch = EvaluationVarsBaseBatch::new(
                xs_batch.len(),
                &local_constants_batch,
                &local_wires_batch,
                public_inputs_hash,
            );

            let mut quotient_values_batch = eval_vanishing_poly_base_batch::<F, C, D>(
                common_data,
                &indices_batch,
                &shifted_xs_batch,
                vars_batch,
                &local_zs_batch,
                &next_zs_batch,
                &partial_products_batch,
                &s_sigmas_batch,
                betas,
                gammas,
                alphas,
                &z_h_on_coset,
            );

            for (&i, quotient_values) in indices_batch.iter().zip(quotient_values_batch.iter_mut())
            {
                let denominator_inv = z_h_on_coset.eval_inverse(i);
                quotient_values
                    .iter_mut()
                    .for_each(|v| *v *= denominator_inv);

                if i == 1048576 {
                    println!("i: {}, res: {:?}", i, quotient_values);
                }
            }
            quotient_values_batch
        })
        .collect()
    );

    println!("quotient_values len:{}, itemLen:{}", quotient_values.len(), quotient_values[0].len());
    // unsafe
    // {
    //     let mut file = File::create("quotient_values.bin").unwrap();
    //     let v = quotient_values.concat();
    //     file.write_all(std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len()*8));
    // }

    let values = timed!(
        timing,
        "transpose",
        transpose(&quotient_values));

    let res: Vec<PolynomialCoeffs<F>> = timed!(
        timing,
        "coset ifft",
        values.into_par_iter()
            .map(PolynomialValues::new)
            .map(|values| values.coset_ifft(F::coset_shift()))
            .collect()
    );

    // unsafe
    // {
    //     let mut file = File::create("quotient_values2.bin").unwrap();
    //     let v = res.iter().flat_map(|f|f.coeffs.clone()).collect::<Vec<_>>();
    //     file.write_all(std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len()*8));
    // }

    // let n_inv = F::inverse_2exp(21);
    // println!("n_inv with 21: {:?}", n_inv);
    // println!("v1: {:?}, v2: {:?}", res[0].coeffs[1048576], res[1].coeffs[1048576]);
    res
}
