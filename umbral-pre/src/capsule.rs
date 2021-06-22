use alloc::vec::Vec;
use core::fmt;

use generic_array::sequence::Concat;
use generic_array::GenericArray;
use typenum::op;

use crate::capsule_frag::CapsuleFrag;
use crate::curve::{CurvePoint, CurveScalar};
use crate::hashing_ds::{hash_capsule_points, hash_to_polynomial_arg, hash_to_shared_secret};
use crate::keys::{PublicKey, SecretKey};
use crate::params::Parameters;
use crate::traits::{
    fmt_public, ConstructionError, DeserializableFromArray, HasTypeName, RepresentableAsArray,
    SerializableToArray,
};

/// Errors that can happen when opening a `Capsule` using reencrypted `CapsuleFrag` objects.
#[derive(Debug, PartialEq)]
pub enum OpenReencryptedError {
    /// An empty capsule fragment list is given.
    NoCapsuleFrags,
    /// Capsule fragments are mismatched (originated from [`KeyFrag`](crate::KeyFrag) objects
    /// generated by different [`generate_kfrags`](crate::generate_kfrags) calls).
    MismatchedCapsuleFrags,
    /// Some of the given capsule fragments are repeated.
    RepeatingCapsuleFrags,
    /// An internally hashed value is zero.
    /// See [rust-umbral#39](https://github.com/nucypher/rust-umbral/issues/39).
    ZeroHash,
    /// Internal validation of the result has failed.
    /// Can be caused by an incorrect (possibly modified) capsule
    /// or some of the capsule fragments.
    ValidationFailed,
}

impl fmt::Display for OpenReencryptedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCapsuleFrags => write!(f, "Empty CapsuleFrag sequence"),
            Self::MismatchedCapsuleFrags => write!(f, "CapsuleFrags are not pairwise consistent"),
            Self::RepeatingCapsuleFrags => write!(f, "Some of the CapsuleFrags are repeated"),
            // Will be removed when #39 is fixed
            Self::ZeroHash => write!(f, "An internally hashed value is zero"),
            Self::ValidationFailed => write!(f, "Internal validation failed"),
        }
    }
}

/// Encapsulated symmetric key used to encrypt the plaintext.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capsule {
    pub(crate) params: Parameters,
    pub(crate) point_e: CurvePoint,
    pub(crate) point_v: CurvePoint,
    pub(crate) signature: CurveScalar,
}

type PointSize = <CurvePoint as RepresentableAsArray>::Size;
type ScalarSize = <CurveScalar as RepresentableAsArray>::Size;

impl RepresentableAsArray for Capsule {
    type Size = op!(PointSize + PointSize + ScalarSize);
}

impl SerializableToArray for Capsule {
    fn to_array(&self) -> GenericArray<u8, Self::Size> {
        self.point_e
            .to_array()
            .concat(self.point_v.to_array())
            .concat(self.signature.to_array())
    }
}

impl DeserializableFromArray for Capsule {
    fn from_array(arr: &GenericArray<u8, Self::Size>) -> Result<Self, ConstructionError> {
        let (point_e, rest) = CurvePoint::take(*arr)?;
        let (point_v, rest) = CurvePoint::take(rest)?;
        let signature = CurveScalar::take_last(rest)?;
        Self::new_verified(point_e, point_v, signature)
            .ok_or_else(|| ConstructionError::new("Capsule", "Self-verification failed"))
    }
}

impl HasTypeName for Capsule {
    fn type_name() -> &'static str {
        "Capsule"
    }
}

impl fmt::Display for Capsule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_public::<Self>(self, f)
    }
}

impl Capsule {
    fn new(point_e: CurvePoint, point_v: CurvePoint, signature: CurveScalar) -> Self {
        let params = Parameters::new();
        Self {
            params,
            point_e,
            point_v,
            signature,
        }
    }

    pub(crate) fn new_verified(
        point_e: CurvePoint,
        point_v: CurvePoint,
        signature: CurveScalar,
    ) -> Option<Self> {
        let capsule = Self::new(point_e, point_v, signature);
        match capsule.verify() {
            false => None,
            true => Some(capsule),
        }
    }

    /// Verifies the integrity of the capsule.
    fn verify(&self) -> bool {
        let g = CurvePoint::generator();
        let h = hash_capsule_points(&self.point_e, &self.point_v);
        &g * &self.signature == &self.point_v + &(&self.point_e * &h)
    }

    /// Generates a symmetric key and its associated KEM ciphertext
    pub(crate) fn from_public_key(delegating_pk: &PublicKey) -> (Capsule, CurvePoint) {
        let g = CurvePoint::generator();

        let priv_r = CurveScalar::random_nonzero();
        let pub_r = &g * &priv_r;

        let priv_u = CurveScalar::random_nonzero();
        let pub_u = &g * &priv_u;

        let h = hash_capsule_points(&pub_r, &pub_u);

        let s = &priv_u + &(&priv_r * &h);

        let shared_key = &delegating_pk.to_point() * &(&priv_r + &priv_u);

        let capsule = Self::new(pub_r, pub_u, s);

        (capsule, shared_key)
    }

    /// Derive the same symmetric key
    pub(crate) fn open_original(&self, delegating_sk: &SecretKey) -> CurvePoint {
        &(&self.point_e + &self.point_v) * delegating_sk.to_secret_scalar().as_secret()
    }

    #[allow(clippy::many_single_char_names)]
    pub(crate) fn open_reencrypted(
        &self,
        receiving_sk: &SecretKey,
        delegating_pk: &PublicKey,
        cfrags: &[CapsuleFrag],
    ) -> Result<CurvePoint, OpenReencryptedError> {
        if cfrags.is_empty() {
            return Err(OpenReencryptedError::NoCapsuleFrags);
        }

        let precursor = cfrags[0].precursor;

        if !cfrags.iter().all(|cfrag| cfrag.precursor == precursor) {
            return Err(OpenReencryptedError::MismatchedCapsuleFrags);
        }

        let pub_key = receiving_sk.public_key().to_point();
        let dh_point = &precursor * receiving_sk.to_secret_scalar().as_secret();

        // Combination of CFrags via Shamir's Secret Sharing reconstruction
        let mut lc = Vec::<CurveScalar>::with_capacity(cfrags.len());
        for cfrag in cfrags {
            let coeff = hash_to_polynomial_arg(&precursor, &pub_key, &dh_point, &cfrag.kfrag_id);
            lc.push(coeff);
        }

        let mut e_prime = CurvePoint::identity();
        let mut v_prime = CurvePoint::identity();
        for (i, cfrag) in (&cfrags).iter().enumerate() {
            // There is a minuscule probability that coefficients for two different frags are equal,
            // in which case we'd rather fail gracefully.
            let lambda_i =
                lambda_coeff(&lc, i).ok_or(OpenReencryptedError::RepeatingCapsuleFrags)?;
            e_prime = &e_prime + &(&cfrag.point_e1 * &lambda_i);
            v_prime = &v_prime + &(&cfrag.point_v1 * &lambda_i);
        }

        // Secret value 'd' allows to make Umbral non-interactive
        let d = hash_to_shared_secret(&precursor, &pub_key, &dh_point);

        let s = self.signature;
        let h = hash_capsule_points(&self.point_e, &self.point_v);

        let orig_pub_key = delegating_pk.to_point();

        // Have to convert from subtle::CtOption here.
        let inv_d_opt: Option<CurveScalar> = d.invert().into();
        // At the moment we cannot guarantee statically that the digest `d` is non-zero.
        // Technically, it is supposed to be non-zero by the choice of `precursor`,
        // but if is was somehow replaced by an incorrect value,
        // we'd rather fail gracefully than panic.
        let inv_d = inv_d_opt.ok_or(OpenReencryptedError::ZeroHash)?;

        if &orig_pub_key * &(&s * &inv_d) != &(&e_prime * &h) + &v_prime {
            return Err(OpenReencryptedError::ValidationFailed);
        }

        let shared_key = &(&e_prime + &v_prime) * &d;
        Ok(shared_key)
    }
}

fn lambda_coeff(xs: &[CurveScalar], i: usize) -> Option<CurveScalar> {
    let mut res = CurveScalar::one();
    for j in 0..xs.len() {
        if j != i {
            let inv_diff_opt: Option<CurveScalar> = (&xs[j] - &xs[i]).invert().into();
            let inv_diff = inv_diff_opt?;
            res = &(&res * &xs[j]) * &inv_diff;
        }
    }
    Some(res)
}

#[cfg(test)]
mod tests {

    use alloc::vec::Vec;

    use super::{Capsule, OpenReencryptedError};
    use crate::{
        encrypt, generate_kfrags, reencrypt, DeserializableFromArray, SecretKey,
        SerializableToArray, Signer,
    };

    #[test]
    fn test_serialize() {
        let delegating_sk = SecretKey::random();
        let delegating_pk = delegating_sk.public_key();

        let plaintext = b"peace at dawn";
        let (capsule, _ciphertext) = encrypt(&delegating_pk, plaintext).unwrap();

        let capsule_arr = capsule.to_array();
        let capsule_back = Capsule::from_array(&capsule_arr).unwrap();
        assert_eq!(capsule, capsule_back);
    }

    #[test]
    fn test_open_reencrypted() {
        let delegating_sk = SecretKey::random();
        let delegating_pk = delegating_sk.public_key();

        let signing_sk = SecretKey::random();
        let signer = Signer::new(&signing_sk);

        let receiving_sk = SecretKey::random();
        let receiving_pk = receiving_sk.public_key();

        let (capsule, key_seed) = Capsule::from_public_key(&delegating_pk);

        let kfrags = generate_kfrags(&delegating_sk, &receiving_pk, &signer, 2, 3, true, true);

        let vcfrags: Vec<_> = kfrags
            .iter()
            .map(|kfrag| reencrypt(&capsule, &kfrag))
            .collect();

        let cfrags: Vec<_> = vcfrags.iter().cloned().map(|vcfrag| vcfrag.cfrag).collect();

        let key_seed_reenc = capsule
            .open_reencrypted(&receiving_sk, &delegating_pk, &cfrags)
            .unwrap();
        assert_eq!(key_seed, key_seed_reenc);

        // Empty cfrag vector
        assert_eq!(
            capsule.open_reencrypted(&receiving_sk, &delegating_pk, &[]),
            Err(OpenReencryptedError::NoCapsuleFrags)
        );

        // Mismatched cfrags - each `generate_kfrags()` uses new randoms.
        let kfrags2 = generate_kfrags(&delegating_sk, &receiving_pk, &signer, 2, 3, true, true);

        let vcfrags2: Vec<_> = kfrags2
            .iter()
            .map(|kfrag| reencrypt(&capsule, &kfrag))
            .collect();

        let mismatched_cfrags: Vec<_> = vcfrags[0..1]
            .iter()
            .cloned()
            .chain(vcfrags2[1..2].iter().cloned())
            .map(|vcfrag| vcfrag.cfrag)
            .collect();

        assert_eq!(
            capsule.open_reencrypted(&receiving_sk, &delegating_pk, &mismatched_cfrags),
            Err(OpenReencryptedError::MismatchedCapsuleFrags)
        );

        // Mismatched capsule
        let (capsule2, _key_seed) = Capsule::from_public_key(&delegating_pk);
        assert_eq!(
            capsule2.open_reencrypted(&receiving_sk, &delegating_pk, &cfrags),
            Err(OpenReencryptedError::ValidationFailed)
        );
    }
}
