use bellman::plonk::polynomials::{Coefficients, Polynomial};
use bellman::worker::Worker;
use bellman::PrimeField;
use commitment::hash::Digest;
use commitment::merkle_tree::verify_multi_branch;
use ff_utils::ff_utils::{FromBytes, ScalarOps, ToBytes};
use fri::{
  fri::verify_low_degree_proof,
  utils::{get_pseudorandom_indices, parse_bytes_to_u64_vec},
};
#[allow(unused_imports)]
use log::{debug, info};
use num::bigint::BigUint;
use std::io::Error;

use crate::utils::*;

pub fn verify_r1cs_proof<T: PrimeField + ScalarOps + FromBytes + ToBytes, H: Digest>(
  proof: StarkProof<H>,
  public_wires: &[T],
  public_first_indices: &[(usize, usize)],
  permuted_indices: &[usize],
  coefficients: &[T],
  flag0: &[T],
  flag1: &[T],
  flag2: &[T],
  n_constraints: usize,
  n_wires: usize,
) -> Result<bool, Error> {
  let original_steps = coefficients.len();
  assert!(original_steps <= 3 * n_constraints * n_wires);
  assert!(original_steps % 3 == 0);

  let log_steps = log2_ceil(original_steps - 1);
  let mut steps = 2usize.pow(log_steps);
  if steps < 8 {
    steps = 8;
  }

  let precision = steps * EXTENSION_FACTOR;
  let log_precision = log_steps + LOG_EXTENSION_FACTOR as u32;
  let log_max_precision = calc_max_log_precision::<T>();
  assert!(precision <= 2usize.pow(log_max_precision));

  let mut permuted_indices = permuted_indices.to_vec();
  permuted_indices.extend(original_steps..steps);
  // println!("permuted_indices: {:?}", permuted_indices);

  let mut coefficients = coefficients.to_vec();
  coefficients.extend(vec![T::zero(); steps - original_steps]);
  let coefficients = Polynomial::from_values(coefficients).unwrap();

  // start_time = time.time()
  let StarkProof {
    m_root,
    l_root,
    a_root,
    main_branches,
    linear_comb_branches,
    fri_proof,
  } = proof;

  // Get (steps)th root of unity
  let ff_order = T::zero() - T::one();
  let times_nmr = BigUint::from_bytes_le(&ff_order.to_bytes_le().unwrap());
  let times_dnm = BigUint::from_bytes_le(&precision.to_le_bytes());
  assert!(&times_nmr % &times_dnm == BigUint::from(0u8));
  let times = parse_bytes_to_u64_vec(&(times_nmr / times_dnm).to_bytes_le()); // (modulus - 1) / precision
  let g2 = T::multiplicative_generator().pow(&times); // g2^precision = 1 mod modulus

  // let xs = expand_root_of_unity(g2);
  let worker = Worker::new();
  let xs = {
    let coeffs = vec![T::zero(); precision];
    let mut xs = Polynomial::from_coeffs(coeffs).unwrap();
    xs.distribute_powers(&worker, g2);
    xs.into_coeffs()
  };
  let skips = precision / steps; // EXTENSION_FACTOR
  let g1 = xs[skips];
  let log_order_of_g1 = log_steps as u32;
  let log_order_of_g2 = log_precision as u32;

  // Interpolate the computational trace into a polynomial P, with each step
  // along a successive power of g1
  println!("calculate expanding polynomials");

  let worker = Worker::new();

  let k_polynomial = coefficients.ifft(&worker);
  println!("Converted coefficients into a polynomial and low-degree extended it");

  let flag0 = Polynomial::from_values(flag0.to_vec()).unwrap();
  let f0_polynomial = flag0.ifft(&worker);
  let flag1 = Polynomial::from_values(flag1.to_vec()).unwrap();
  let f1_polynomial = flag1.ifft(&worker);
  let flag2 = Polynomial::from_values(flag2.to_vec()).unwrap();
  let f2_polynomial = flag2.ifft(&worker);
  println!("Converted flags into a polynomial and low-degree extended it");

  // Verifies the low-degree proofs
  assert!(
    verify_low_degree_proof(l_root.clone(), g2, &fri_proof, precision / 4, skips as u32).unwrap()
  );

  let positions = get_pseudorandom_indices(
    l_root.as_ref(),
    precision as u32,
    SPOT_CHECK_SECURITY_FACTOR,
    skips as u32,
  )
  .iter()
  .map(|&i| i as usize)
  .collect::<Vec<usize>>();
  let mut augmented_positions = vec![];
  for &j in positions.iter().peekable() {
    // println!(
    //   "{:?} {:?} {:?} {:?}",
    //   j,
    //   (j + precision - skips) % precision,
    //   (j + original_steps / 3 * skips) % precision,
    //   (j + 2 * original_steps / 3 * skips) % precision,
    // );
    augmented_positions.extend([
      j,
      (j + precision - skips) % precision,
      (j + original_steps / 3 * skips) % precision,
      (j + 2 * original_steps / 3 * skips) % precision,
    ]);
  }
  // println!("positions: {:?}", positions);

  // Performs the spot checks
  let main_branches = main_branches;
  let main_branch_leaves =
    verify_multi_branch(&m_root, &augmented_positions, main_branches).unwrap();
  let linear_comb_branch_leaves =
    verify_multi_branch(&l_root, &positions, linear_comb_branches).unwrap();

  let mut z_polynomial = calc_z_polynomial(steps).unwrap();
  z_polynomial.pad_to_size(precision).unwrap();
  let z_evaluations = z_polynomial.fft(&worker);

  // let z3_polynomial = calc_z_polynomial(steps);
  // let z3_evaluations = best_fft(z3_polynomial, &g2, log_order_of_g2);

  let converted_indices: Vec<T> = convert_usize_iter_to_ff_vec(0..steps);
  let mut index_polynomial = Polynomial::from_values(converted_indices)
    .unwrap()
    .ifft(&worker);
  index_polynomial.pad_to_size(precision).unwrap();
  let ext_indices = index_polynomial.fft(&worker);
  println!("Computed extended index polynomial");

  let converted_permuted_indices: Vec<T> = convert_usize_iter_to_ff_vec(permuted_indices.clone());
  let mut permuted_polynomial = Polynomial::from_values(converted_permuted_indices)
    .unwrap()
    .ifft(&worker);
  permuted_polynomial.pad_to_size(precision).unwrap();
  let ext_permuted_indices = permuted_polynomial.fft(&worker);
  // println!("ext_permuted_indices: {:?}", ext_permuted_indices);
  println!("Computed extended permuted index polynomial");

  // let interpolant = {
  //   let mut x_vals = vec![];
  //   let mut y_vals = vec![];
  //   for (j, n_coeff) in n_coeff_list[0..public_wires.len()]
  //     .to_vec()
  //     .iter()
  //     .enumerate()
  //   {
  //     x_vals.push(xs[n_coeff * skips]);
  //     y_vals.push(public_wires[j]);
  //   }

  //   lagrange_interp(&x_vals, &y_vals)
  // };

  let interpolant2 = calc_i2_polynomial(public_first_indices, &xs, &public_wires, skips).unwrap();

  let x_of_last_step = xs[(steps - 1) * skips];
  let interpolant3 = calc_i3_polynomial(&xs, skips).unwrap();
  println!("Computed boundary polynomial");

  let r: Vec<T> = get_random_ff_values(a_root.as_ref(), precision as u32, 3, 0);
  // println!("r: {:?}", r);

  // let k0 = T::one();
  // let k1 = T::from_str(&mk_seed(&[m_root.as_ref().to_vec(), b"\x01".to_vec()])).unwrap();
  // ...
  // let k10 = T::from_str(&mk_seed(&[m_root.as_ref().to_vec(), b"\x0a".to_vec()])).unwrap();
  let mut k = vec![T::one()];
  for i in 1u8..11 {
    k.push(
      T::from_str(&mk_seed(&[
        m_root.as_ref().to_vec(),
        i.to_be_bytes().to_vec(),
      ]))
      .unwrap(),
    );
  }

  for (i, &pos) in positions.iter().enumerate() {
    let x = xs[pos]; // g2.pow_vartime(&parse_bytes_to_u64_vec(&pos.to_le_bytes()));

    let m_branch0 = main_branch_leaves[i * 4].chunks(32).collect::<Vec<_>>();
    let m_branch1 = main_branch_leaves[i * 4 + 1].chunks(32).collect::<Vec<_>>();
    let m_branch2 = main_branch_leaves[i * 4 + 2].chunks(32).collect::<Vec<_>>();
    let m_branch3 = main_branch_leaves[i * 4 + 3].chunks(32).collect::<Vec<_>>();

    let p_of_x = T::from_bytes_le(m_branch0[0]).unwrap();
    let p_of_prev_x = T::from_bytes_le(m_branch1[0]).unwrap();
    let p_of_x_plus_w = T::from_bytes_le(m_branch2[0]).unwrap();
    let p_of_x_plus_2w = T::from_bytes_le(m_branch3[0]).unwrap();
    let a_of_x = T::from_bytes_le(m_branch0[1]).unwrap();
    let a_of_prev_x = T::from_bytes_le(m_branch1[1]).unwrap();
    let s_of_x = T::from_bytes_le(m_branch0[2]).unwrap();
    let d1_of_x = T::from_bytes_le(m_branch0[3]).unwrap();
    let d2_of_x = T::from_bytes_le(m_branch0[4]).unwrap();
    let d3_of_x = T::from_bytes_le(m_branch0[5]).unwrap();
    let b_of_x = T::from_bytes_le(m_branch0[6]).unwrap();
    let b3_of_x = T::from_bytes_le(m_branch0[7]).unwrap();

    let z_value: T = z_evaluations.as_ref()[pos];
    // let z2_value = z2_evaluations[pos];
    // let z3_value = z3_evaluations[pos];

    let k_of_x = k_polynomial.evaluate_at(&worker, x);
    let f0 = f0_polynomial.evaluate_at(&worker, x);
    let f1 = f1_polynomial.evaluate_at(&worker, x);
    let f2 = f2_polynomial.evaluate_at(&worker, x);

    // Check first transition constraints Q1(x) = Z1(x) * D1(x)
    assert_eq!(
      f0 * (p_of_x - f1 * p_of_prev_x - k_of_x * s_of_x),
      z_value * d1_of_x
    );

    // Check second transition constraints Q2(x) = Z2(x) * D2(x)
    assert_eq!(
      f2 * (p_of_x_plus_2w - p_of_x * p_of_x_plus_w),
      z_value * d2_of_x
    );

    let val_nmr: T = r[0] + r[1] * ext_indices.as_ref()[pos] + r[2] * s_of_x;
    let val_dnm: T = r[0] + r[1] * ext_permuted_indices.as_ref()[pos] + r[2] * s_of_x;

    // Check third transition constraints Q3(x) = Z3(x) * D3(x)
    assert_eq!(a_of_x * val_dnm - a_of_prev_x * val_nmr, z_value * d3_of_x);

    // Check boundary constraints P(x) - I(x) = Zb(x) * B(x)
    let mut zb2_of_x = T::one();
    for (_, w) in public_first_indices {
      let v = x - xs[w * skips];
      zb2_of_x.mul_assign(&v);
    }
    let i2_of_x = interpolant2.evaluate_at(&worker, x);
    assert_eq!(s_of_x - i2_of_x, zb2_of_x * b_of_x);

    let zb3_of_x = x - x_of_last_step;
    let i3_of_x = interpolant3.evaluate_at(&worker, x);
    assert_eq!(a_of_x - i3_of_x, zb3_of_x * b3_of_x);

    // Check correctness of the linear combination
    let x_to_the_steps = x.pow(&parse_bytes_to_u64_vec(&steps.to_le_bytes()));
    let l_of_x = T::from_bytes_le(&linear_comb_branch_leaves[i]).unwrap();
    assert_eq!(
      l_of_x,
      k[0] * d1_of_x
        + k[1] * d2_of_x
        + k[2] * d3_of_x
        + k[3] * p_of_x
        + k[4] * p_of_x * x_to_the_steps
        + k[5] * b_of_x
        + k[6] * b_of_x * x_to_the_steps
        + k[7] * b3_of_x
        + k[8] * b3_of_x * x_to_the_steps
        + k[9] * a_of_x
        + k[10] * s_of_x,
    );
  }

  println!("Verified {} consistency checks", SPOT_CHECK_SECURITY_FACTOR);
  Ok(true)
}
