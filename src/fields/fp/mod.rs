use ark_ff::{BigInteger, PrimeField};
use ark_relations::gr1cs::{
    ConstraintSystemRef, LinearCombination, Namespace, SynthesisError, Variable,
};
use ark_std::{borrow::Borrow, iter::Sum, vec::Vec};
use itertools::zip_eq;

use crate::{boolean::AllocatedBool, convert::ToConstraintFieldGadget, prelude::*, Assignment};

mod cmp;

/// Represents a variable in the constraint system whose
/// value can be an arbitrary field element.
#[derive(Debug, Clone)]
#[must_use]
pub struct AllocatedFp<F: PrimeField> {
    pub(crate) value: Option<F>,
    /// The allocated variable corresponding to `self` in `self.cs`.
    pub variable: Variable,
    /// The constraint system that `self` was allocated in.
    pub cs: ConstraintSystemRef<F>,
}

impl<F: PrimeField> AllocatedFp<F> {
    /// Constructs a new `AllocatedFp` from a (optional) value, a low-level
    /// Variable, and a `ConstraintSystemRef`.
    pub fn new(value: Option<F>, variable: Variable, cs: ConstraintSystemRef<F>) -> Self {
        Self {
            value,
            variable,
            cs,
        }
    }
}

/// Represent variables corresponding to a field element in `F`.
#[derive(Clone, Debug)]
#[must_use]
pub enum FpVar<F: PrimeField> {
    /// Represents a constant in the constraint system, which means that
    /// it does not have a corresponding variable.
    Constant(F),
    /// Represents an allocated variable constant in the constraint system.
    Var(AllocatedFp<F>),
}

impl<F: PrimeField> FpVar<F> {
    /// Decomposes `self` into a vector of `bits` and a remainder `rest` such
    /// that
    /// * `bits.len() == size`, and
    /// * `rest == 0`.
    pub fn to_bits_le_with_top_bits_zero(
        &self,
        size: usize,
    ) -> Result<(Vec<Boolean<F>>, Self), SynthesisError> {
        assert!(size < F::MODULUS_BIT_SIZE as usize);
        let cs = self.cs();
        let mode = if self.is_constant() {
            AllocationMode::Constant
        } else {
            AllocationMode::Witness
        };

        let value = self.value().map(|f| f.into_bigint());
        let lower_bits = (0..size)
            .map(|i| {
                Boolean::new_variable(cs.clone(), || value.map(|v| v.get_bit(i as usize)), mode)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let lower_bits_fp = Boolean::le_bits_to_fp(&lower_bits)?;
        let rest = self - &lower_bits_fp;
        rest.enforce_equal(&Self::zero())?;
        Ok((lower_bits, rest))
    }
}

impl<F: PrimeField> GR1CSVar<F> for FpVar<F> {
    type Value = F;

    fn cs(&self) -> ConstraintSystemRef<F> {
        match self {
            Self::Constant(_) => ConstraintSystemRef::None,
            Self::Var(a) => a.cs.clone(),
        }
    }

    fn value(&self) -> Result<Self::Value, SynthesisError> {
        match self {
            Self::Constant(v) => Ok(*v),
            Self::Var(v) => v.value(),
        }
    }
}

impl<F: PrimeField> From<Boolean<F>> for FpVar<F> {
    fn from(other: Boolean<F>) -> Self {
        if let Boolean::Constant(b) = other {
            Self::Constant(F::from(b as u8))
        } else {
            // `other` is a variable
            let cs = other.cs();
            let variable = cs.new_lc(|| other.lc()).unwrap();
            Self::Var(AllocatedFp::new(
                other.value().ok().map(|b| F::from(b as u8)),
                variable,
                cs,
            ))
        }
    }
}

impl<F: PrimeField> From<AllocatedFp<F>> for FpVar<F> {
    fn from(other: AllocatedFp<F>) -> Self {
        Self::Var(other)
    }
}

impl<'a, F: PrimeField> FieldOpsBounds<'a, F, Self> for FpVar<F> {}
impl<'a, F: PrimeField> FieldOpsBounds<'a, F, FpVar<F>> for &'a FpVar<F> {}

impl<F: PrimeField> AllocatedFp<F> {
    /// Constructs `Self` from a `Boolean`: if `other` is false, this outputs
    /// `zero`, else it outputs `one`.
    pub fn from(other: Boolean<F>) -> Self {
        let cs = other.cs();
        let variable = cs.new_lc(|| other.lc()).unwrap();
        Self::new(other.value().ok().map(|b| F::from(b as u8)), variable, cs)
    }

    /// Returns the value assigned to `self` in the underlying constraint system
    /// (if a value was assigned).
    pub fn value(&self) -> Result<F, SynthesisError> {
        self.value.ok_or(SynthesisError::AssignmentMissing)
    }

    /// Outputs `self + other`.
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn add(&self, other: &Self) -> Self {
        let value = match (self.value, other.value) {
            (Some(val1), Some(val2)) => Some(val1 + &val2),
            (..) => None,
        };

        let variable = self
            .cs
            .new_lc(|| lc![self.variable, other.variable])
            .unwrap();
        AllocatedFp::new(value, variable, self.cs.clone())
    }

    /// Add many allocated Fp elements together.
    ///
    /// This does not create any constraints and only creates one linear
    /// combination.
    ///
    /// Returns `None` if you pass an empty iterator.
    pub fn add_many<B: Borrow<Self>>(iter: &[B]) -> Option<Self> {
        let mut has_value = true;
        let mut value = F::zero();
        let mut cs = ConstraintSystemRef::None;

        let mut num_iters = 0;

        for variable in iter {
            let variable = variable.borrow();
            cs = cs.or(variable.cs.clone());
            if variable.value.is_none() {
                has_value = false;
            } else {
                value += variable.value.unwrap();
            }
            num_iters += 1;
        }
        if num_iters == 0 {
            return None; // No elements to add
        }

        let variable = cs
            .new_lc(|| {
                let lc = iter
                    .iter()
                    .map(|variable| (F::ONE, variable.borrow().variable))
                    .collect();
                let mut lc = LinearCombination(lc);
                lc.compactify();
                lc
            })
            .unwrap();
        if has_value {
            Some(AllocatedFp::new(Some(value), variable, cs))
        } else {
            Some(AllocatedFp::new(None, variable, cs))
        }
    }

    /// Computes the inner product of two iterators of `AllocatedFp` elements.
    ///
    ///
    /// This does not create any constraints and only creates one linear
    /// combination.
    ///
    /// # Panics
    ///
    /// Panics if the iterators are of different lengths.
    pub fn linear_combination<B1, B2, I1>(this: I1, other: &[B2]) -> Option<Self>
    where
        B1: Borrow<F>,
        B2: Borrow<Self>,
        I1: IntoIterator<Item = B1, IntoIter: Clone>,
    {
        let mut cs = ConstraintSystemRef::None;
        let mut has_value = true;
        let mut value = F::zero();

        let mut num_iters = 0;
        let zipped = zip_eq(this, other);
        for (coeff, variable) in zipped.clone() {
            let coeff = *coeff.borrow();
            let variable = variable.borrow();
            cs = cs.or(variable.cs.clone());
            if variable.value.is_none() {
                has_value = false;
            } else {
                value += coeff * variable.value.unwrap();
            }
            num_iters += 1;
        }
        if num_iters == 0 {
            return None; // No elements to add
        }

        let variable = cs
            .new_lc(|| {
                let lc = zipped
                    .map(|(coeff, variable)| (*coeff.borrow(), variable.borrow().variable))
                    .collect::<Vec<_>>();
                let mut lc = LinearCombination(lc);
                // sorts and compacts
                lc.compactify();
                lc
            })
            .unwrap();

        if has_value {
            Some(AllocatedFp::new(Some(value), variable, cs))
        } else {
            Some(AllocatedFp::new(None, variable, cs))
        }
    }

    /// Computes the inner product of two iterators of `AllocatedFp` elements.
    ///
    ///
    /// This does not create any constraints and only creates one linear
    /// combination.
    ///
    /// # Panics
    ///
    /// Panics if the iterators are of different lengths.
    pub fn inner_product<B1, B2, I1, I2>(this: I1, other: I2) -> Option<Self>
    where
        B1: Borrow<Self>,
        B2: Borrow<Self>,
        I1: IntoIterator<Item = B1>,
        I2: IntoIterator<Item = B2>,
    {
        let mut cs = ConstraintSystemRef::None;
        let mut has_value = true;
        let mut value = F::zero();
        let this = this.into_iter();
        let mut new_lc = Vec::with_capacity(this.size_hint().0);

        let mut num_iters = 0;
        for (v1, v2) in zip_eq(this, other) {
            let v1 = v1.borrow();
            let v2 = v2.borrow();
            cs = cs.or(v1.cs.clone()).or(v2.cs.clone());
            match (v1.value, v2.value) {
                (Some(val1), Some(val2)) => value += val1 * val2,
                (..) => has_value = false,
            }
            if v1.cs.is_none() && v2.cs.is_none() {
                // both v1 and v2 should be constants
                let v1 = v1.value?;
                let v2 = v2.value?;
                let product = v1 * v2;
                new_lc.push((product, Variable::One));
            }
            if v1.cs.is_none() {
                // v1 should be a constant
                let v1 = v1.value?;
                new_lc.push((v1, v2.variable));
            } else if v2.cs.is_none() {
                // v2 should be a constant
                let v2 = v2.value?;
                new_lc.push((v2, v1.variable));
            } else {
                let product = v1.mul(v2);
                new_lc.push((F::ONE, product.variable));
            }
            num_iters += 1;
        }
        if num_iters == 0 {
            return None; // No elements to compute the inner product
        }
        let variable = cs
            .new_lc(|| {
                let mut lc = LinearCombination(new_lc);
                // sorts and compacts
                lc.compactify();
                lc
            })
            .unwrap();

        if has_value {
            Some(AllocatedFp::new(Some(value), variable, cs))
        } else {
            Some(AllocatedFp::new(None, variable, cs))
        }
    }

    /// Outputs `self - other`.
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn sub(&self, other: &Self) -> Self {
        let value = match (self.value, other.value) {
            (Some(val1), Some(val2)) => Some(val1 - &val2),
            (..) => None,
        };

        let variable = self
            .cs
            .new_lc(|| lc_diff![self.variable, other.variable])
            .unwrap();
        AllocatedFp::new(value, variable, self.cs.clone())
    }

    /// Outputs `self * other`.
    ///
    /// This requires *one* constraint.
    #[tracing::instrument(target = "gr1cs")]
    pub fn mul(&self, other: &Self) -> Self {
        let product = AllocatedFp::new_witness(self.cs.clone(), || {
            Ok(self.value.get()? * &other.value.get()?)
        })
        .unwrap();
        self.cs
            .enforce_r1cs_constraint(
                || self.variable.into(),
                || other.variable.into(),
                || product.variable.into(),
            )
            .unwrap();
        product
    }

    /// Output `self + other`
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn add_constant(&self, other: F) -> Self {
        if other.is_zero() {
            self.clone()
        } else {
            let value = self.value.map(|val| val + other);
            let variable = self
                .cs
                .new_lc(|| lc![(F::ONE, self.variable), (other, Variable::One)])
                .unwrap();
            AllocatedFp::new(value, variable, self.cs.clone())
        }
    }

    /// Output `self - other`
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn sub_constant(&self, other: F) -> Self {
        self.add_constant(-other)
    }

    /// Output `self * other`
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn mul_constant(&self, other: F) -> Self {
        if other.is_one() {
            self.clone()
        } else {
            let value = self.value.map(|val| val * other);
            let variable = self.cs.new_lc(|| (other, self.variable).into()).unwrap();
            AllocatedFp::new(value, variable, self.cs.clone())
        }
    }

    /// Output `self + self`
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn double(&self) -> Result<Self, SynthesisError> {
        let value = self.value.map(|val| val.double());
        let variable = self.cs.new_lc(|| (F::ONE.double(), self.variable).into())?;
        Ok(Self::new(value, variable, self.cs.clone()))
    }

    /// Output `-self`
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn negate(&self) -> Self {
        let mut result = self.clone();
        result.negate_in_place();
        result
    }

    /// Sets `self = -self`
    ///
    /// This does not create any constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn negate_in_place(&mut self) -> &mut Self {
        if let Some(val) = self.value.as_mut() {
            *val = -(*val);
        }
        self.variable = self.cs.new_lc(|| lc!() - self.variable).unwrap();
        self
    }

    /// Outputs `self * self`
    ///
    /// This requires *one* constraint.
    #[tracing::instrument(target = "gr1cs")]
    pub fn square(&self) -> Result<Self, SynthesisError> {
        Ok(self.mul(self))
    }

    /// Outputs `result` such that `result * self = 1`.
    ///
    /// This requires *one* constraint.
    #[tracing::instrument(target = "gr1cs")]
    pub fn inverse(&self) -> Result<Self, SynthesisError> {
        let inverse = Self::new_witness(self.cs.clone(), || {
            Ok(self.value.get()?.inverse().unwrap_or(F::ZERO))
        })?;

        self.cs.enforce_r1cs_constraint(
            || self.variable.into(),
            || inverse.variable.into(),
            || Variable::One.into(),
        )?;
        Ok(inverse)
    }

    /// This is a no-op for prime fields.
    #[tracing::instrument(target = "gr1cs")]
    pub fn frobenius_map(&self, _: usize) -> Result<Self, SynthesisError> {
        Ok(self.clone())
    }

    /// Enforces that `self * other = result`.
    ///
    /// This requires *one* constraint.
    #[tracing::instrument(target = "gr1cs")]
    pub fn mul_equals(&self, other: &Self, result: &Self) -> Result<(), SynthesisError> {
        self.cs.enforce_r1cs_constraint(
            || self.variable.into(),
            || other.variable.into(),
            || result.variable.into(),
        )
    }

    /// Enforces that `self * self = result`.
    ///
    /// This requires *one* constraint.
    #[tracing::instrument(target = "gr1cs")]
    pub fn square_equals(&self, result: &Self) -> Result<(), SynthesisError> {
        self.cs.enforce_r1cs_constraint(
            || self.variable.into(),
            || self.variable.into(),
            || result.variable.into(),
        )
    }

    /// Outputs the bit `self == other`.
    ///
    /// This requires two constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn is_eq(&self, other: &Self) -> Result<Boolean<F>, SynthesisError> {
        self.is_neq(other).map(core::ops::Not::not)
    }

    /// Outputs the bit `self != other`.
    ///
    /// This requires two constraints.
    #[tracing::instrument(target = "gr1cs")]
    pub fn is_neq(&self, other: &Self) -> Result<Boolean<F>, SynthesisError> {
        // We don't need to enforce `is_not_equal` to be boolean here;
        // see the comments above the constraints below for why.
        let is_not_equal = Boolean::from(AllocatedBool::new_witness_without_booleanity_check(
            self.cs.clone(),
            || Ok(self.value.get()? != other.value.get()?),
        )?);
        let multiplier = self.cs.new_witness_variable(|| {
            let self_value = self.value.get()?;
            let other_value = other.value.get()?;
            if self_value != other_value {
                Ok((self_value - other_value).inverse().unwrap_or(F::ZERO))
            } else {
                Ok(F::one())
            }
        })?;

        // Completeness:
        // Case 1: self != other:
        // ----------------------
        //   constraint 1:
        //   (self - other) * multiplier = is_not_equal
        //   => (non_zero) * multiplier = 1 (satisfied, because multiplier = 1/(self -
        // other)
        //
        //   constraint 2:
        //   (self - other) * not(is_not_equal) = 0
        //   => (non_zero) * not(1) = 0
        //   => (non_zero) * 0 = 0
        //
        // Case 2: self == other:
        // ----------------------
        //   constraint 1:
        //   (self - other) * multiplier = is_not_equal
        //   => 0 * multiplier = 0 (satisfied, because multiplier = 1
        //
        //   constraint 2:
        //   (self - other) * not(is_not_equal) = 0
        //   => 0 * not(0) = 0
        //   => 0 * 1 = 0
        //
        // --------------------------------------------------------------------
        //
        // Soundness:
        // Case 1: self != other, but is_not_equal != 1.
        // --------------------------------------------
        //   constraint 2:
        //   (self - other) * not(is_not_equal) = 0
        //   => (non_zero) * (1 - is_not_equal) = 0
        //   => non_zero = 0 (contradiction) || 1 - is_not_equal = 0 (contradiction)
        //
        // Case 2: self == other, but is_not_equal != 0.
        // --------------------------------------------
        //   constraint 1:
        //   (self - other) * multiplier = is_not_equal
        //   0 * multiplier = is_not_equal != 0 (unsatisfiable)
        //
        // That is, constraint 1 enforces that if self == other, then `is_not_equal = 0`
        // and constraint 2 enforces that if self != other, then `is_not_equal = 1`.
        // Since these are the only possible two cases, `is_not_equal` is always
        // constrained to 0 or 1.
        let difference = self.cs.new_lc(|| lc_diff![self.variable, other.variable])?;
        self.cs.enforce_r1cs_constraint(
            || difference.into(),
            || multiplier.into(),
            || is_not_equal.lc(),
        )?;
        let is_equal = !&is_not_equal;
        self.cs
            .enforce_r1cs_constraint(|| difference.into(), || is_equal.lc(), || lc!())?;
        Ok(is_not_equal)
    }

    /// Enforces that self == other if `should_enforce.is_eq(&Boolean::TRUE)`.
    ///
    /// This requires one constraint.
    #[tracing::instrument(target = "gr1cs")]
    pub fn conditional_enforce_equal(
        &self,
        other: &Self,
        should_enforce: &Boolean<F>,
    ) -> Result<(), SynthesisError> {
        self.cs.enforce_r1cs_constraint(
            || lc_diff![self.variable, other.variable],
            || should_enforce.lc(),
            || lc!(),
        )
    }

    /// Enforces that self != other if `should_enforce.is_eq(&Boolean::TRUE)`.
    ///
    /// This requires one constraint.
    #[tracing::instrument(target = "gr1cs")]
    pub fn conditional_enforce_not_equal(
        &self,
        other: &Self,
        should_enforce: &Boolean<F>,
    ) -> Result<(), SynthesisError> {
        // The high level logic is as follows:
        // We want to check that self - other != 0. We do this by checking that
        // (self - other).inverse() exists. In more detail, we check the following:
        // If `should_enforce == true`, then we set `multiplier = (self -
        // other).inverse()`, and check that (self - other) * multiplier == 1.
        // (i.e., that the inverse exists)
        //
        // If `should_enforce == false`, then we set `multiplier == 0`, and check that
        // (self - other) * 0 == 0, which is always satisfied.
        let multiplier = Self::new_witness(self.cs.clone(), || {
            if should_enforce.value()? {
                Ok((self.value.get()? - other.value.get()?)
                    .inverse()
                    .unwrap_or(F::ZERO))
            } else {
                Ok(F::zero())
            }
        })?;

        self.cs.enforce_r1cs_constraint(
            || lc_diff![self.variable, other.variable],
            || multiplier.variable.into(),
            || should_enforce.lc(),
        )?;
        Ok(())
    }
}

/// *************************************************************************
/// *************************************************************************

impl<F: PrimeField> ToBitsGadget<F> for AllocatedFp<F> {
    /// Outputs the unique bit-wise decomposition of `self` in *little-endian*
    /// form.
    ///
    /// This method enforces that the output is in the field, i.e.
    /// it invokes `Boolean::enforce_in_field_le` on the bit decomposition.
    #[tracing::instrument(target = "gr1cs")]
    fn to_bits_le(&self) -> Result<Vec<Boolean<F>>, SynthesisError> {
        let bits = self.to_non_unique_bits_le()?;
        Boolean::enforce_in_field_le(&bits)?;
        Ok(bits)
    }

    #[tracing::instrument(target = "gr1cs")]
    fn to_non_unique_bits_le(&self) -> Result<Vec<Boolean<F>>, SynthesisError> {
        let cs = self.cs.clone();
        use ark_ff::BitIteratorBE;
        let mut bits = if let Some(value) = self.value {
            let field_char = BitIteratorBE::new(F::characteristic());
            let bits: Vec<_> = BitIteratorBE::new(value.into_bigint())
                .zip(field_char)
                .skip_while(|(_, c)| !c)
                .map(|(b, _)| Some(b))
                .collect();
            assert_eq!(bits.len(), F::MODULUS_BIT_SIZE as usize);
            bits
        } else {
            vec![None; F::MODULUS_BIT_SIZE as usize]
        };

        // Convert to little-endian
        bits.reverse();

        let bits: Vec<_> = bits
            .into_iter()
            .map(|b| Boolean::new_witness(cs.clone(), || b.get()))
            .collect::<Result<_, _>>()?;

        let lc = || {
            let mut coeff = F::one();
            let lc = bits
                .iter()
                .map(|bit| {
                    let c = coeff;
                    coeff.double_in_place();
                    (c, bit.variable())
                })
                .chain([(-F::ONE, self.variable)])
                .collect::<Vec<_>>();
            let mut lc = LinearCombination(lc);
            lc.compactify();
            lc
        };

        cs.enforce_r1cs_constraint(|| lc!(), || lc!(), lc)?;

        Ok(bits)
    }
}

impl<F: PrimeField> ToBytesGadget<F> for AllocatedFp<F> {
    /// Outputs the unique byte decomposition of `self` in *little-endian*
    /// form.
    ///
    /// This method enforces that the decomposition represents
    /// an integer that is less than `F::MODULUS`.
    #[tracing::instrument(target = "gr1cs")]
    fn to_bytes_le(&self) -> Result<Vec<UInt8<F>>, SynthesisError> {
        let num_bits = F::BigInt::NUM_LIMBS * 64;
        let mut bits = self.to_bits_le()?;
        let remainder = core::iter::repeat(Boolean::FALSE).take(num_bits - bits.len());
        bits.extend(remainder);
        let bytes = bits
            .chunks(8)
            .map(|chunk| UInt8::from_bits_le(chunk))
            .collect();
        Ok(bytes)
    }

    #[tracing::instrument(target = "gr1cs")]
    fn to_non_unique_bytes_le(&self) -> Result<Vec<UInt8<F>>, SynthesisError> {
        let num_bits = F::BigInt::NUM_LIMBS * 64;
        let mut bits = self.to_non_unique_bits_le()?;
        let remainder = core::iter::repeat(Boolean::FALSE).take(num_bits - bits.len());
        bits.extend(remainder);
        let bytes = bits
            .chunks(8)
            .map(|chunk| UInt8::from_bits_le(chunk))
            .collect();
        Ok(bytes)
    }
}

impl<F: PrimeField> ToConstraintFieldGadget<F> for AllocatedFp<F> {
    #[tracing::instrument(target = "gr1cs")]
    fn to_constraint_field(&self) -> Result<Vec<FpVar<F>>, SynthesisError> {
        Ok(vec![self.clone().into()])
    }
}

impl<F: PrimeField> CondSelectGadget<F> for AllocatedFp<F> {
    #[inline]
    #[tracing::instrument(target = "gr1cs")]
    fn conditionally_select(
        cond: &Boolean<F>,
        true_val: &Self,
        false_val: &Self,
    ) -> Result<Self, SynthesisError> {
        match cond {
            &Boolean::Constant(true) => Ok(true_val.clone()),
            &Boolean::Constant(false) => Ok(false_val.clone()),
            _ => {
                let cs = cond.cs();
                let result = Self::new_witness(cs.clone(), || {
                    cond.value()
                        .and_then(|c| if c { true_val } else { false_val }.value.get())
                })?;
                // a = self; b = other; c = cond;
                //
                // r = c * a + (1  - c) * b
                // r = b + c * (a - b)
                // c * (a - b) = r - b
                cs.enforce_r1cs_constraint(
                    || cond.lc(),
                    || lc_diff![true_val.variable, false_val.variable],
                    || lc_diff![result.variable, false_val.variable],
                )?;

                Ok(result)
            },
        }
    }
}

/// Uses two bits to perform a lookup into a table
/// `b` is little-endian: `b[0]` is LSB.
impl<F: PrimeField> TwoBitLookupGadget<F> for AllocatedFp<F> {
    type TableConstant = F;
    #[tracing::instrument(target = "gr1cs")]
    fn two_bit_lookup(b: &[Boolean<F>], c: &[Self::TableConstant]) -> Result<Self, SynthesisError> {
        debug_assert_eq!(b.len(), 2);
        debug_assert_eq!(c.len(), 4);
        let result = Self::new_witness(b.cs(), || {
            let lsb = usize::from(b[0].value()?);
            let msb = usize::from(b[1].value()?);
            let index = lsb + (msb << 1);
            Ok(c[index])
        })?;
        let one = Variable::One;
        b.cs().enforce_r1cs_constraint(
            || b[1].lc() * (c[3] - &c[2] - &c[1] + &c[0]) + (c[1] - &c[0], one),
            || b[0].lc(),
            || lc!() + result.variable - (c[0], one) + b[1].lc() * (c[0] - &c[2]),
        )?;

        Ok(result)
    }
}

impl<F: PrimeField> ThreeBitCondNegLookupGadget<F> for AllocatedFp<F> {
    type TableConstant = F;

    #[tracing::instrument(target = "gr1cs")]
    fn three_bit_cond_neg_lookup(
        b: &[Boolean<F>],
        b0b1: &Boolean<F>,
        c: &[Self::TableConstant],
    ) -> Result<Self, SynthesisError> {
        debug_assert_eq!(b.len(), 3);
        debug_assert_eq!(c.len(), 4);
        let result = Self::new_witness(b.cs(), || {
            let lsb = usize::from(b[0].value()?);
            let msb = usize::from(b[1].value()?);
            let index = lsb + (msb << 1);
            let intermediate = c[index];

            let is_negative = b[2].value()?;
            let y = if is_negative {
                -intermediate
            } else {
                intermediate
            };
            Ok(y)
        })?;

        // enforce y * (1 - 2 * b_2) == res
        b.cs().enforce_r1cs_constraint(
            || {
                b0b1.lc() * (c[3] - &c[2] - &c[1] + &c[0])
                    + b[0].lc() * (c[1] - &c[0])
                    + b[1].lc() * (c[2] - &c[0])
                    + (c[0], Variable::One)
            },
            || b[2].lc() * F::from(2u64).neg() + (F::one(), Variable::One),
            || result.variable.into(),
        )?;

        Ok(result)
    }
}

impl<F: PrimeField> AllocVar<F, F> for AllocatedFp<F> {
    fn new_variable<T: Borrow<F>>(
        cs: impl Into<Namespace<F>>,
        f: impl FnOnce() -> Result<T, SynthesisError>,
        mode: AllocationMode,
    ) -> Result<Self, SynthesisError> {
        let ns = cs.into();
        let cs = ns.cs();
        if mode == AllocationMode::Constant {
            let v = *f()?.borrow();
            let lc = cs.new_lc(|| (v, Variable::One).into())?;
            Ok(Self::new(Some(v), lc, cs))
        } else {
            let mut value = None;
            let value_generator = || {
                value = Some(*f()?.borrow());
                value.ok_or(SynthesisError::AssignmentMissing)
            };
            let variable = if mode == AllocationMode::Input {
                cs.new_input_variable(value_generator)?
            } else {
                cs.new_witness_variable(value_generator)?
            };
            Ok(Self::new(value, variable, cs))
        }
    }
}

impl<F: PrimeField> FieldVar<F, F> for FpVar<F> {
    fn constant(f: F) -> Self {
        Self::Constant(f)
    }

    fn zero() -> Self {
        Self::Constant(F::zero())
    }

    fn one() -> Self {
        Self::Constant(F::one())
    }

    #[tracing::instrument(target = "gr1cs")]
    fn double(&self) -> Result<Self, SynthesisError> {
        match self {
            Self::Constant(c) => Ok(Self::Constant(c.double())),
            Self::Var(v) => Ok(Self::Var(v.double()?)),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn negate(&self) -> Result<Self, SynthesisError> {
        match self {
            Self::Constant(c) => Ok(Self::Constant(-*c)),
            Self::Var(v) => Ok(Self::Var(v.negate())),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn square(&self) -> Result<Self, SynthesisError> {
        match self {
            Self::Constant(c) => Ok(Self::Constant(c.square())),
            Self::Var(v) => Ok(Self::Var(v.square()?)),
        }
    }

    /// Enforce that `self * other == result`.
    #[tracing::instrument(target = "gr1cs")]
    fn mul_equals(&self, other: &Self, result: &Self) -> Result<(), SynthesisError> {
        use FpVar::*;
        match (self, other, result) {
            (Constant(_), Constant(_), Constant(_)) => Ok(()),
            (Constant(_), Constant(_), _) | (Constant(_), Var(_), _) | (Var(_), Constant(_), _) => {
                result.enforce_equal(&(self * other))
            }, // this multiplication should be free
            (Var(v1), Var(v2), Var(v3)) => v1.mul_equals(v2, v3),
            (Var(v1), Var(v2), Constant(f)) => {
                let cs = v1.cs.clone();
                let v3 = AllocatedFp::new_constant(cs, f).unwrap();
                v1.mul_equals(v2, &v3)
            },
        }
    }

    /// Enforce that `self * self == result`.
    #[tracing::instrument(target = "gr1cs")]
    fn square_equals(&self, result: &Self) -> Result<(), SynthesisError> {
        use FpVar::*;
        match (self, result) {
            (Constant(_), Constant(_)) => Ok(()),
            (Constant(f), Var(r)) => {
                let cs = r.cs.clone();
                let v = AllocatedFp::new_witness(cs, || Ok(f))?;
                v.square_equals(&r)
            },
            (Var(v), Constant(f)) => {
                let cs = v.cs.clone();
                let r = AllocatedFp::new_witness(cs, || Ok(f))?;
                v.square_equals(&r)
            },
            (Var(v1), Var(v2)) => v1.square_equals(v2),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn inverse(&self) -> Result<Self, SynthesisError> {
        match self {
            FpVar::Var(v) => v.inverse().map(FpVar::Var),
            FpVar::Constant(f) => f.inverse().get().map(FpVar::Constant),
        }
    }

    /// Computes the inner product of two slices of `FpVar`.
    /// This is faster for the `ConstraintSystem` to process as it directly creates
    /// the minimal number of linear combinations.
    #[tracing::instrument(target = "gr1cs")]
    fn inner_product(this: &[Self], other: &[Self]) -> Result<Self, SynthesisError> {
        if this.len() != other.len() {
            return Err(SynthesisError::Unsatisfiable);
        }

        let mut lc_vars = vec![];
        let mut lc_coeffs = vec![];
        let mut sum_constants = F::zero();
        // constants, linear_combinations, and variables separately
        let (vars_left, vars_right): (Vec<_>, Vec<_>) = this
            .iter()
            .zip(other)
            .filter_map(|(x, y)| match (x, y) {
                (FpVar::Constant(x), FpVar::Constant(y)) => {
                    // If both are constants, we can sum them directly
                    sum_constants += *x * y;
                    None
                },
                (FpVar::Constant(x), FpVar::Var(y)) | (FpVar::Var(y), FpVar::Constant(x)) => {
                    // If one is a constant, we can treat it as a linear combination
                    lc_vars.push(y);
                    lc_coeffs.push(*x);
                    None
                },
                // If both are variables, we keep them for the inner product
                (FpVar::Var(x), FpVar::Var(y)) => Some((x, y)),
            })
            .unzip();
        let sum_constants = FpVar::Constant(sum_constants);
        let sum_lc = AllocatedFp::linear_combination(lc_coeffs, &lc_vars).map(FpVar::Var);
        let sum_variables = AllocatedFp::inner_product(vars_left, vars_right).map(FpVar::Var);

        match (sum_lc, sum_variables) {
            (Some(a), Some(b)) => Ok(a + b + sum_constants),
            (Some(a), None) | (None, Some(a)) => Ok(a + sum_constants),
            (None, None) => Ok(sum_constants),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn frobenius_map(&self, power: usize) -> Result<Self, SynthesisError> {
        match self {
            FpVar::Var(v) => v.frobenius_map(power).map(FpVar::Var),
            FpVar::Constant(f) => {
                let mut f = *f;
                f.frobenius_map_in_place(power);
                Ok(FpVar::Constant(f))
            },
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn frobenius_map_in_place(&mut self, power: usize) -> Result<&mut Self, SynthesisError> {
        *self = self.frobenius_map(power)?;
        Ok(self)
    }
}

impl_ops!(
    FpVar<F>,
    F,
    Add,
    add,
    AddAssign,
    add_assign,
    |this: &'a FpVar<F>, other: &'a FpVar<F>| {
        use FpVar::*;
        match (this, other) {
            (Constant(c1), Constant(c2)) => Constant(*c1 + *c2),
            (Constant(c), Var(v)) | (Var(v), Constant(c)) => Var(v.add_constant(*c)),
            (Var(v1), Var(v2)) => Var(v1.add(v2)),
        }
    },
    |this: &'a FpVar<F>, other: F| { this + &FpVar::Constant(other) },
    F: PrimeField,
);

impl_ops!(
    FpVar<F>,
    F,
    Sub,
    sub,
    SubAssign,
    sub_assign,
    |this: &'a FpVar<F>, other: &'a FpVar<F>| {
        use FpVar::*;
        match (this, other) {
            (Constant(c1), Constant(c2)) => Constant(*c1 - *c2),
            (Var(v), Constant(c)) => Var(v.sub_constant(*c)),
            (Constant(c), Var(v)) => Var(v.sub_constant(*c).negate()),
            (Var(v1), Var(v2)) => Var(v1.sub(v2)),
        }
    },
    |this: &'a FpVar<F>, other: F| { this - &FpVar::Constant(other) },
    F: PrimeField
);

impl_ops!(
    FpVar<F>,
    F,
    Mul,
    mul,
    MulAssign,
    mul_assign,
    |this: &'a FpVar<F>, other: &'a FpVar<F>| {
        use FpVar::*;
        match (this, other) {
            (Constant(c1), Constant(c2)) => Constant(*c1 * *c2),
            (Constant(c), Var(v)) | (Var(v), Constant(c)) => Var(v.mul_constant(*c)),
            (Var(v1), Var(v2)) => Var(v1.mul(v2)),
        }
    },
    |this: &'a FpVar<F>, other: F| {
        if other.is_zero() {
            FpVar::zero()
        } else {
            this * &FpVar::Constant(other)
        }
    },
    F: PrimeField
);

/// *************************************************************************
/// *************************************************************************

impl<F: PrimeField> EqGadget<F> for FpVar<F> {
    #[tracing::instrument(target = "gr1cs")]
    fn is_eq(&self, other: &Self) -> Result<Boolean<F>, SynthesisError> {
        match (self, other) {
            (Self::Constant(c1), Self::Constant(c2)) => Ok(Boolean::Constant(c1 == c2)),
            (Self::Constant(c), Self::Var(v)) | (Self::Var(v), Self::Constant(c)) => {
                let cs = v.cs.clone();
                let c = AllocatedFp::new_constant(cs, c)?;
                c.is_eq(v)
            },
            (Self::Var(v1), Self::Var(v2)) => v1.is_eq(v2),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn conditional_enforce_equal(
        &self,
        other: &Self,
        should_enforce: &Boolean<F>,
    ) -> Result<(), SynthesisError> {
        match (self, other) {
            (Self::Constant(_), Self::Constant(_)) => Ok(()),
            (Self::Constant(c), Self::Var(v)) | (Self::Var(v), Self::Constant(c)) => {
                let cs = v.cs.clone();
                let c = AllocatedFp::new_constant(cs, c)?;
                c.conditional_enforce_equal(v, should_enforce)
            },
            (Self::Var(v1), Self::Var(v2)) => v1.conditional_enforce_equal(v2, should_enforce),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn conditional_enforce_not_equal(
        &self,
        other: &Self,
        should_enforce: &Boolean<F>,
    ) -> Result<(), SynthesisError> {
        match (self, other) {
            (Self::Constant(_), Self::Constant(_)) => Ok(()),
            (Self::Constant(c), Self::Var(v)) | (Self::Var(v), Self::Constant(c)) => {
                let cs = v.cs.clone();
                let c = AllocatedFp::new_constant(cs, c)?;
                c.conditional_enforce_not_equal(v, should_enforce)
            },
            (Self::Var(v1), Self::Var(v2)) => v1.conditional_enforce_not_equal(v2, should_enforce),
        }
    }
}

impl<F: PrimeField> ToBitsGadget<F> for FpVar<F> {
    #[tracing::instrument(target = "gr1cs")]
    fn to_bits_le(&self) -> Result<Vec<Boolean<F>>, SynthesisError> {
        match self {
            Self::Constant(_) => self.to_non_unique_bits_le(),
            Self::Var(v) => v.to_bits_le(),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn to_non_unique_bits_le(&self) -> Result<Vec<Boolean<F>>, SynthesisError> {
        use ark_ff::BitIteratorLE;
        match self {
            Self::Constant(c) => Ok(BitIteratorLE::new(&c.into_bigint())
                .take((F::MODULUS_BIT_SIZE) as usize)
                .map(Boolean::constant)
                .collect::<Vec<_>>()),
            Self::Var(v) => v.to_non_unique_bits_le(),
        }
    }
}

impl<F: PrimeField> ToBytesGadget<F> for FpVar<F> {
    /// Outputs the unique byte decomposition of `self` in *little-endian*
    /// form.
    #[tracing::instrument(target = "gr1cs")]
    fn to_bytes_le(&self) -> Result<Vec<UInt8<F>>, SynthesisError> {
        match self {
            Self::Constant(c) => Ok(UInt8::constant_vec(
                c.into_bigint().to_bytes_le().as_slice(),
            )),
            Self::Var(v) => v.to_bytes_le(),
        }
    }

    #[tracing::instrument(target = "gr1cs")]
    fn to_non_unique_bytes_le(&self) -> Result<Vec<UInt8<F>>, SynthesisError> {
        match self {
            Self::Constant(c) => Ok(UInt8::constant_vec(
                c.into_bigint().to_bytes_le().as_slice(),
            )),
            Self::Var(v) => v.to_non_unique_bytes_le(),
        }
    }
}

impl<F: PrimeField> ToConstraintFieldGadget<F> for FpVar<F> {
    #[tracing::instrument(target = "gr1cs")]
    fn to_constraint_field(&self) -> Result<Vec<FpVar<F>>, SynthesisError> {
        Ok(vec![self.clone()])
    }
}

impl<F: PrimeField> CondSelectGadget<F> for FpVar<F> {
    #[tracing::instrument(target = "gr1cs")]
    fn conditionally_select(
        cond: &Boolean<F>,
        true_value: &Self,
        false_value: &Self,
    ) -> Result<Self, SynthesisError> {
        match cond {
            &Boolean::Constant(true) => Ok(true_value.clone()),
            &Boolean::Constant(false) => Ok(false_value.clone()),
            _ => {
                match (true_value, false_value) {
                    (Self::Constant(t), Self::Constant(f)) => {
                        let is = AllocatedFp::from(cond.clone());
                        let not = AllocatedFp::from(!cond);
                        // cond * t + (1 - cond) * f
                        Ok(is.mul_constant(*t).add(&not.mul_constant(*f)).into())
                    },
                    (..) => {
                        let cs = cond.cs();
                        let true_value = match true_value {
                            Self::Constant(f) => AllocatedFp::new_constant(cs.clone(), f)?,
                            Self::Var(v) => v.clone(),
                        };
                        let false_value = match false_value {
                            Self::Constant(f) => AllocatedFp::new_constant(cs, f)?,
                            Self::Var(v) => v.clone(),
                        };
                        cond.select(&true_value, &false_value).map(Self::Var)
                    },
                }
            },
        }
    }
}

/// Uses two bits to perform a lookup into a table
/// `b` is little-endian: `b[0]` is LSB.
impl<F: PrimeField> TwoBitLookupGadget<F> for FpVar<F> {
    type TableConstant = F;

    #[tracing::instrument(target = "gr1cs")]
    fn two_bit_lookup(b: &[Boolean<F>], c: &[Self::TableConstant]) -> Result<Self, SynthesisError> {
        debug_assert_eq!(b.len(), 2);
        debug_assert_eq!(c.len(), 4);
        if b.is_constant() {
            let lsb = usize::from(b[0].value()?);
            let msb = usize::from(b[1].value()?);
            let index = lsb + (msb << 1);
            Ok(Self::Constant(c[index]))
        } else {
            AllocatedFp::two_bit_lookup(b, c).map(Self::Var)
        }
    }
}

impl<F: PrimeField> ThreeBitCondNegLookupGadget<F> for FpVar<F> {
    type TableConstant = F;

    #[tracing::instrument(target = "gr1cs")]
    fn three_bit_cond_neg_lookup(
        b: &[Boolean<F>],
        b0b1: &Boolean<F>,
        c: &[Self::TableConstant],
    ) -> Result<Self, SynthesisError> {
        debug_assert_eq!(b.len(), 3);
        debug_assert_eq!(c.len(), 4);

        if b.cs().or(b0b1.cs()).is_none() {
            // We only have constants

            let lsb = usize::from(b[0].value()?);
            let msb = usize::from(b[1].value()?);
            let index = lsb + (msb << 1);
            let intermediate = c[index];

            let is_negative = b[2].value()?;
            let y = if is_negative {
                -intermediate
            } else {
                intermediate
            };
            Ok(Self::Constant(y))
        } else {
            AllocatedFp::three_bit_cond_neg_lookup(b, b0b1, c).map(Self::Var)
        }
    }
}

impl<F: PrimeField> AllocVar<F, F> for FpVar<F> {
    fn new_variable<T: Borrow<F>>(
        cs: impl Into<Namespace<F>>,
        f: impl FnOnce() -> Result<T, SynthesisError>,
        mode: AllocationMode,
    ) -> Result<Self, SynthesisError> {
        if mode == AllocationMode::Constant {
            Ok(Self::Constant(*f()?.borrow()))
        } else {
            AllocatedFp::new_variable(cs, f, mode).map(Self::Var)
        }
    }
}

impl<'a, F: PrimeField> Sum<&'a FpVar<F>> for FpVar<F> {
    fn sum<I: Iterator<Item = &'a FpVar<F>>>(iter: I) -> FpVar<F> {
        let mut sum_constants = F::zero();
        let variables: Vec<_> = iter
            .filter_map(|x| match x {
                FpVar::Constant(c) => {
                    sum_constants += c;
                    None
                },
                FpVar::Var(v) => Some(v),
            })
            .collect();
        // Can't use `AllocatedFp::add_many` with an empty iterator: it panics.
        if variables.is_empty() {
            return FpVar::Constant(sum_constants);
        }
        AllocatedFp::add_many(&variables).map_or(FpVar::Constant(sum_constants), |sum_vars| {
            FpVar::Var(sum_vars) + sum_constants
        })
    }
}

impl<'a, F: PrimeField> Sum<FpVar<F>> for FpVar<F> {
    fn sum<I: Iterator<Item = FpVar<F>>>(iter: I) -> FpVar<F> {
        let mut sum_constants = F::zero();
        let variables: Vec<_> = iter
            .filter_map(|x| match x {
                FpVar::Constant(c) => {
                    sum_constants += c;
                    None
                },
                FpVar::Var(v) => Some(v),
            })
            .collect();
        // Can't use `AllocatedFp::add_many` with an empty iterator: it panics.
        if variables.is_empty() {
            return FpVar::Constant(sum_constants);
        }
        AllocatedFp::add_many(&variables).map_or(FpVar::Constant(sum_constants), |sum_vars| {
            FpVar::Var(sum_vars) + sum_constants
        })
    }
}

#[cfg(test)]
mod test {
    use crate::{
        alloc::AllocVar,
        eq::EqGadget,
        fields::{fp::FpVar, FieldVar},
        test_utils::{combination, modes},
        GR1CSVar,
    };
    use ark_relations::gr1cs::ConstraintSystem;
    use ark_std::{UniformRand, Zero};
    use ark_test_curves::bls12_381::Fr;

    #[test]
    fn test_inner_product() {
        let mut rng = ark_std::test_rng();
        let cs = ConstraintSystem::new_ref();

        for (a_mode, b_mode) in combination(modes()) {
            let a = (0..10)
                .map(|_| FpVar::new_variable(cs.clone(), || Ok(Fr::rand(&mut rng)), a_mode).ok())
                .collect::<Option<Vec<_>>>()
                .unwrap();
            let b = (0..10)
                .map(|_| FpVar::new_variable(cs.clone(), || Ok(Fr::rand(&mut rng)), b_mode).ok())
                .collect::<Option<Vec<_>>>()
                .unwrap();
            let a = [a, b].concat();
            let b = a.iter().rev().cloned().collect::<Vec<_>>();
            let inner_product: FpVar<Fr> = FpVar::inner_product(&a, &b).unwrap();
            let mut expected = Fr::zero();
            for (x, y) in a.iter().zip(b) {
                expected += x.value().unwrap() * y.value().unwrap();
            }
            inner_product
                .enforce_equal(&FpVar::Constant(expected))
                .unwrap();

            assert!(cs.is_satisfied().unwrap());
        }
    }

    #[test]
    fn test_sum_fpvar() {
        let mut rng = ark_std::test_rng();
        let cs = ConstraintSystem::new_ref();

        for (a_mode, b_mode) in combination(modes()) {
            let a = (0..10)
                .map(|_| FpVar::new_variable(cs.clone(), || Ok(Fr::rand(&mut rng)), a_mode).ok())
                .collect::<Option<Vec<_>>>()
                .unwrap();
            let b = (0..10)
                .map(|_| FpVar::new_variable(cs.clone(), || Ok(Fr::rand(&mut rng)), b_mode).ok())
                .collect::<Option<Vec<_>>>()
                .unwrap();
            let v = [a, b].concat();
            let sum: FpVar<Fr> = v.iter().sum();

            let sum_expected = v.iter().map(|x| x.value().unwrap()).sum();
            sum.enforce_equal(&FpVar::Constant(sum_expected)).unwrap();

            assert!(cs.is_satisfied().unwrap());
            assert_eq!(sum.value().unwrap(), sum_expected);
        }
    }
}
