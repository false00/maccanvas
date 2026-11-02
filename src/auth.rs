use std::io::{Read, Write};

use aes::cipher::{BlockEncrypt, KeyInit};
use crypto_bigint::{
    BoxedUint,
    modular::{BoxedMontyForm, BoxedMontyParams},
};
use md5::{Digest, Md5};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    error::{Error, Result},
    profiles::AuthMode,
};

#[derive(Clone)]
pub struct Credentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

impl Credentials {
    pub fn new(username: String, password: String) -> Self {
        Self {
            username: Zeroizing::new(username),
            password: Zeroizing::new(password),
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }
    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn validate(&self, mode: AuthMode) -> Result<()> {
        if self.username.contains('\0') || self.password.contains('\0') || self.password.is_empty()
        {
            return Err(Error::Authentication(
                "Enter a password without NUL characters".into(),
            ));
        }
        match mode {
            AuthMode::MacLogin if self.username.is_empty()
                || self.username.len() >= 64 || self.password.len() >= 64 =>
                Err(Error::Authentication("Mac login requires a username and password, each below 64 UTF-8 bytes".into())),
            AuthMode::VncPassword if self.password.len() > 8 =>
                Err(Error::Authentication("Classic VNC passwords are limited to 8 bytes; use Mac login for your macOS account password".into())),
            _ => Ok(()),
        }
    }
}

pub fn mac_login(stream: &mut (impl Read + Write), credentials: &Credentials) -> Result<()> {
    credentials.validate(AuthMode::MacLogin)?;
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let generator = u16::from_be_bytes([header[0], header[1]]);
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    if !(128..=1024).contains(&length) || generator < 2 {
        return Err(Error::Protocol(
            "Unsupported Apple DH key length or generator".into(),
        ));
    }
    let mut modulus_bytes = vec![0; length];
    let mut peer_bytes = vec![0; length];
    stream.read_exact(&mut modulus_bytes)?;
    stream.read_exact(&mut peer_bytes)?;
    let first = modulus_bytes
        .iter()
        .position(|&byte| byte != 0)
        .ok_or_else(|| Error::Protocol("Invalid DH modulus".into()))?;
    let modulus_bits = (length - first) * 8 - modulus_bytes[first].leading_zeros() as usize;
    if modulus_bits < 1024 || modulus_bytes[length - 1] & 1 == 0 {
        return Err(Error::Protocol(
            "The Mac supplied an invalid or weak DH modulus".into(),
        ));
    }
    let precision = (length * 8) as u32;
    let decode = |bytes: &[u8]| {
        BoxedUint::from_be_slice(bytes, precision)
            .map_err(|_| Error::Protocol("Invalid DH integer".into()))
    };
    let modulus = decode(&modulus_bytes)?;
    let peer = decode(&peer_bytes)?;
    let one = BoxedUint::one_with_precision(precision);
    let maximum = modulus.wrapping_sub(&one);
    let generator = decode(&generator.to_be_bytes())?;
    if peer <= one || peer >= maximum || generator >= maximum {
        return Err(Error::Protocol("Invalid DH public key".into()));
    }
    let odd = modulus
        .into_odd()
        .into_option()
        .ok_or_else(|| Error::Protocol("Invalid DH modulus".into()))?;
    let parameters = BoxedMontyParams::new_vartime(odd);
    let mut random = Zeroizing::new(vec![0; length]);
    getrandom::fill(&mut random)
        .map_err(|error| Error::Authentication(format!("Secure randomness failed: {error}")))?;
    if random.iter().all(|&byte| byte == 0) {
        return Err(Error::Authentication(
            "Secure randomness produced an invalid exponent".into(),
        ));
    }
    let exponent = Zeroizing::new(decode(&random)?);
    let shared = Zeroizing::new(BoxedMontyForm::new(peer, &parameters).pow(&exponent));
    let shared_integer = Zeroizing::new(shared.retrieve());
    let shared_bytes = Zeroizing::new(shared_integer.to_be_bytes());
    let mut digest = Md5::digest(&shared_bytes[shared_bytes.len() - length..]);
    let mut key = Zeroizing::new([0u8; 16]);
    key.copy_from_slice(&digest);
    digest.as_mut_slice().zeroize();
    let mut packet = Zeroizing::new([0u8; 128]);
    getrandom::fill(&mut *packet)
        .map_err(|error| Error::Authentication(format!("Secure randomness failed: {error}")))?;
    packet[..credentials.username.len()].copy_from_slice(credentials.username.as_bytes());
    packet[credentials.username.len()] = 0;
    packet[64..64 + credentials.password.len()].copy_from_slice(credentials.password.as_bytes());
    packet[64 + credentials.password.len()] = 0;

    // Apple ARD mandates this legacy construction; it is not a storage cipher.
    let cipher = aes::Aes128::new_from_slice(&key[..])
        .map_err(|_| Error::Authentication("Invalid Apple authentication key".into()))?;
    for block in packet.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(block.into());
    }
    let public = BoxedMontyForm::new(generator, &parameters)
        .pow(&exponent)
        .retrieve()
        .to_be_bytes();
    stream.write_all(&packet[..])?;
    stream.write_all(&public[public.len() - length..])?;
    stream.flush()?;
    Ok(())
}

pub fn vnc_password(stream: &mut (impl Read + Write), credentials: &Credentials) -> Result<()> {
    credentials.validate(AuthMode::VncPassword)?;
    let mut key = Zeroizing::new([0u8; 8]);
    for (destination, &byte) in key.iter_mut().zip(credentials.password.as_bytes()) {
        *destination = byte.reverse_bits();
    }
    let mut challenge = Zeroizing::new([0u8; 16]);
    stream.read_exact(&mut *challenge)?;
    let cipher = des::Des::new_from_slice(&key[..])
        .map_err(|_| Error::Authentication("Invalid VNC authentication key".into()))?;
    for block in challenge.as_chunks_mut::<8>().0 {
        cipher.encrypt_block(block.into());
    }
    stream.write_all(&challenge[..])?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_cannot_be_truncated_silently() {
        assert!(
            Credentials::new("user".into(), "p".repeat(64))
                .validate(AuthMode::MacLogin)
                .is_err()
        );
        assert!(
            Credentials::new("".into(), "secret".into())
                .validate(AuthMode::MacLogin)
                .is_err()
        );
        assert!(
            Credentials::new("".into(), "123456789".into())
                .validate(AuthMode::VncPassword)
                .is_err()
        );
        assert!(
            Credentials::new("".into(), "secret".into())
                .validate(AuthMode::VncPassword)
                .is_ok()
        );
    }

    #[test]
    fn rejects_weak_dh_before_allocating_peer_data() {
        struct Duplex(std::io::Cursor<Vec<u8>>);
        impl Read for Duplex {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(bytes)
            }
        }
        impl Write for Duplex {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut stream = Duplex(std::io::Cursor::new(vec![0, 2, 0, 64]));
        let credentials = Credentials::new("fixture-user".into(), "fixture-password".into());
        assert!(mac_login(&mut stream, &credentials).is_err());
    }
}
