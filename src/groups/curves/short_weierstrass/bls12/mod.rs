use ark_ec::{
    bls12::{Bls12Config, G1Prepared, G2Prepared, TwistType},
    short_weierstrass::Affine as GroupAffine,
};
use ark_ff::{BitIteratorBE, Field, One};
use ark_relations::gr1cs::{Namespace, SynthesisError};

use crate::{
    fields::{fp::FpVar, fp2::Fp2Var, FieldVar},
    groups::curves::short_weierstrass::*,
    Vec,
};

/// Represents a projective point in G1.
pub type G1Var<P> = ProjectiveVar<<P as Bls12Config>::G1Config, FpVar<<P as Bls12Config>::Fp>>;

/// Represents an affine point on G1. Should be used only for comparison and
/// when a canonical representation of a point is required, and not for
/// arithmetic.
pub type G1AffineVar<P> = AffineVar<<P as Bls12Config>::G1Config, FpVar<<P as Bls12Config>::Fp>>;

/// Represents a projective point in G2.
pub type G2Var<P> = ProjectiveVar<<P as Bls12Config>::G2Config, Fp2G<P>>;
/// Represents an affine point on G2. Should be used only for comparison and
/// when a canonical representation of a point is required, and not for
/// arithmetic.
pub type G2AffineVar<P> = AffineVar<<P as Bls12Config>::G2Config, Fp2G<P>>;

/// Represents the cached precomputation that can be performed on a G1 element
/// which enables speeding up pairing computation.
#[derive(Educe)]
#[educe(Clone, Debug)]
pub struct G1PreparedVar<P: Bls12Config>(pub AffineVar<P::G1Config, FpVar<P::Fp>>);

impl<P: Bls12Config> G1PreparedVar<P> {
    /// Returns the value assigned to `self` in the underlying constraint
    /// system.
    pub fn value(&self) -> Result<G1Prepared<P>, SynthesisError> {
        let x = self.0.x.value()?;
        let y = self.0.y.value()?;
        let infinity = self.0.infinity.value()?;
        let g = infinity
            .then_some(GroupAffine::identity())
            .unwrap_or(GroupAffine::new(x, y))
            .into();
        Ok(g)
    }

    /// Constructs `Self` from a `G1Var`.
    pub fn from_group_var(q: &G1Var<P>) -> Result<Self, SynthesisError> {
        let g = q.to_affine()?;
        Ok(Self(g))
    }
}

impl<P: Bls12Config> AllocVar<G1Prepared<P>, P::Fp> for G1PreparedVar<P> {
    fn new_variable<T: Borrow<G1Prepared<P>>>(
        cs: impl Into<Namespace<P::Fp>>,
        f: impl FnOnce() -> Result<T, SynthesisError>,
        mode: AllocationMode,
    ) -> Result<Self, SynthesisError> {
        let ns = cs.into();
        let cs = ns.cs();
        let g1_prep = f().map(|b| b.borrow().0);

        let x = FpVar::new_variable(ark_relations::ns!(cs, "x"), || g1_prep.map(|g| g.x), mode)?;
        let y = FpVar::new_variable(ark_relations::ns!(cs, "y"), || g1_prep.map(|g| g.y), mode)?;
        let infinity = Boolean::new_variable(
            ark_relations::ns!(cs, "inf"),
            || g1_prep.map(|g| g.is_zero()),
            mode,
        )?;
        let g = AffineVar::new(x, y, infinity);
        Ok(Self(g))
    }
}

impl<P: Bls12Config> ToBytesGadget<P::Fp> for G1PreparedVar<P> {
    #[inline]
    #[tracing::instrument(target = "gr1cs")]
    fn to_bytes_le(&self) -> Result<Vec<UInt8<P::Fp>>, SynthesisError> {
        let mut bytes = self.0.x.to_bytes_le()?;
        let y_bytes = self.0.y.to_bytes_le()?;
        let inf_bytes = self.0.infinity.to_bytes_le()?;
        bytes.extend_from_slice(&y_bytes);
        bytes.extend_from_slice(&inf_bytes);
        Ok(bytes)
    }

    #[tracing::instrument(target = "gr1cs")]
    fn to_non_unique_bytes_le(&self) -> Result<Vec<UInt8<P::Fp>>, SynthesisError> {
        let mut bytes = self.0.x.to_non_unique_bytes_le()?;
        let y_bytes = self.0.y.to_non_unique_bytes_le()?;
        let inf_bytes = self.0.infinity.to_non_unique_bytes_le()?;
        bytes.extend_from_slice(&y_bytes);
        bytes.extend_from_slice(&inf_bytes);
        Ok(bytes)
    }
}

type Fp2G<P> = Fp2Var<<P as Bls12Config>::Fp2Config>;
type LCoeff<P> = (Fp2G<P>, Fp2G<P>);
/// Represents the cached precomputation that can be performed on a G2 element
/// which enables speeding up pairing computation.
#[derive(Educe)]
#[educe(Clone, Debug)]
pub struct G2PreparedVar<P: Bls12Config> {
    #[doc(hidden)]
    pub ell_coeffs: Vec<LCoeff<P>>,
}

impl<P: Bls12Config> AllocVar<G2Prepared<P>, P::Fp> for G2PreparedVar<P> {
    #[tracing::instrument(target = "gr1cs", skip(cs, f, mode))]
    fn new_variable<T: Borrow<G2Prepared<P>>>(
        cs: impl Into<Namespace<P::Fp>>,
        f: impl FnOnce() -> Result<T, SynthesisError>,
        mode: AllocationMode,
    ) -> Result<Self, SynthesisError> {
        let ns = cs.into();
        let cs = ns.cs();
        let g2_prep = f().map(|b| {
            let projective_coeffs = &b.borrow().ell_coeffs;
            match P::TWIST_TYPE {
                TwistType::M => {
                    let mut z_s = projective_coeffs
                        .iter()
                        .map(|(_, _, z)| *z)
                        .collect::<Vec<_>>();
                    ark_ff::fields::batch_inversion(&mut z_s);
                    projective_coeffs
                        .iter()
                        .zip(z_s)
                        .map(|((x, y, _), z_inv)| (*x * &z_inv, *y * &z_inv))
                        .collect::<Vec<_>>()
                },
                TwistType::D => {
                    let mut z_s = projective_coeffs
                        .iter()
                        .map(|(z, ..)| *z)
                        .collect::<Vec<_>>();
                    ark_ff::fields::batch_inversion(&mut z_s);
                    projective_coeffs
                        .iter()
                        .zip(z_s)
                        .map(|((_, x, y), z_inv)| (*x * &z_inv, *y * &z_inv))
                        .collect::<Vec<_>>()
                },
            }
        });

        let l = Vec::new_variable(
            ark_relations::ns!(cs, "l"),
            || {
                g2_prep
                    .clone()
                    .map(|c| c.iter().map(|(l, _)| *l).collect::<Vec<_>>())
            },
            mode,
        )?;
        let r = Vec::new_variable(
            ark_relations::ns!(cs, "r"),
            || g2_prep.map(|c| c.iter().map(|(_, r)| *r).collect::<Vec<_>>()),
            mode,
        )?;
        let ell_coeffs = l.into_iter().zip(r).collect();
        Ok(Self { ell_coeffs })
    }
}

impl<P: Bls12Config> ToBytesGadget<P::Fp> for G2PreparedVar<P> {
    #[inline]
    #[tracing::instrument(target = "gr1cs")]
    fn to_bytes_le(&self) -> Result<Vec<UInt8<P::Fp>>, SynthesisError> {
        let mut bytes = Vec::new();
        for coeffs in &self.ell_coeffs {
            bytes.extend_from_slice(&coeffs.0.to_bytes_le()?);
            bytes.extend_from_slice(&coeffs.1.to_bytes_le()?);
        }
        Ok(bytes)
    }

    #[tracing::instrument(target = "gr1cs")]
    fn to_non_unique_bytes_le(&self) -> Result<Vec<UInt8<P::Fp>>, SynthesisError> {
        let mut bytes = Vec::new();
        for coeffs in &self.ell_coeffs {
            bytes.extend_from_slice(&coeffs.0.to_non_unique_bytes_le()?);
            bytes.extend_from_slice(&coeffs.1.to_non_unique_bytes_le()?);
        }
        Ok(bytes)
    }
}

impl<P: Bls12Config> G2PreparedVar<P> {
    /// Constructs `Self` from a `G2Var`.
    #[tracing::instrument(target = "gr1cs")]
    pub fn from_group_var(q: &G2Var<P>) -> Result<Self, SynthesisError> {
        let q = q.to_affine()?;
        let two_inv = P::Fp::one().double().inverse().unwrap();
        // Enforce that `q` is not the point at infinity.
        q.infinity.enforce_not_equal(&Boolean::TRUE)?;
        let mut ell_coeffs = vec![];
        let mut r = q.clone();

        for i in BitIteratorBE::new(P::X).skip(1) {
            ell_coeffs.push(Self::double(&mut r, &two_inv)?);

            if i {
                ell_coeffs.push(Self::add(&mut r, &q)?);
            }
        }

        Ok(Self { ell_coeffs })
    }

    #[tracing::instrument(target = "gr1cs")]
    fn double(r: &mut G2AffineVar<P>, two_inv: &P::Fp) -> Result<LCoeff<P>, SynthesisError> {
        let a = r.y.inverse()?;
        let mut b = r.x.square()?;
        let b_tmp = b.clone();
        b.mul_assign_by_base_field_constant(*two_inv);
        b += &b_tmp;

        let c = &a * &b;
        let d = r.x.double()?;
        let x3 = c.square()? - &d;
        let e = &c * &r.x - &r.y;
        let c_x3 = &c * &x3;
        let y3 = &e - &c_x3;
        let mut f = c;
        f.negate_in_place()?;
        r.x = x3;
        r.y = y3;
        match P::TWIST_TYPE {
            TwistType::M => Ok((e, f)),
            TwistType::D => Ok((f, e)),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn add(r: &mut G2AffineVar<P>, q: &G2AffineVar<P>) -> Result<LCoeff<P>, SynthesisError> {
        let a = (&q.x - &r.x).inverse()?;
        let b = &q.y - &r.y;
        let c = &a * &b;
        let d = &r.x + &q.x;
        let x3 = c.square()? - &d;

        let e = (&r.x - &x3) * &c;
        let y3 = e - &r.y;
        let g = &c * &r.x - &r.y;
        let mut f = c;
        f.negate_in_place()?;
        r.x = x3;
        r.y = y3;
        match P::TWIST_TYPE {
            TwistType::M => Ok((g, f)),
            TwistType::D => Ok((f, g)),
        }
    }
}
