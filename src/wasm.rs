//! Functions for extracting and embedding claims within a WebAssembly module

use crate::errors::{self, ErrorKind};
use crate::jwt::Claims;
use crate::jwt::Token;
use crate::Result;
use chrono::Duration;
use data_encoding::HEXUPPER;
use nkeys::KeyPair;
use parity_wasm::elements::CustomSection;
use parity_wasm::{
    deserialize_buffer,
    elements::{Module, Serialize},
    serialize,
};
use ring::digest::{Context, Digest, SHA256};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};
const SECS_PER_DAY: u64 = 86400;

/// Extracts a set of claims from the raw bytes of a WebAssembly module. In the case where no
/// JWT is discovered in the module, this function returns `None`.
/// If there is a token in the file with a valid hash, then you will get a `Token` back
/// containing both the raw JWT and the decoded claims.
///
/// # Errors
/// Will return errors if the file cannot be read, cannot be parsed, contains an improperly
/// forms JWT, or the `module_hash` claim inside the decoded JWT does not match the hash
/// of the file.
pub fn extract_claims(contents: impl AsRef<[u8]>) -> Result<Option<Token>> {
    let module: Module = deserialize_buffer(contents.as_ref())?;

    let sections: Vec<&CustomSection> = module
        .custom_sections()
        .filter(|sect| sect.name() == "jwt")
        .collect();

    if sections.len() == 0 {
        Ok(None)
    } else {
        let jwt = String::from_utf8(sections[0].payload().to_vec())?;
        let claims = Claims::decode(&jwt)?;
        let hash = compute_hash_without_jwt(module)?;

        /* TODO: FIX MODULE HASHING */
        if hash != claims.module_hash {
            Err(errors::new(ErrorKind::InvalidModuleHash))
        } else {
            Ok(Some(Token { jwt, claims }))
        }
    }
}

/// This function will embed a set of claims inside the bytecode of a WebAssembly module. The claims
/// are converted into a JWT and signed using the provided `KeyPair`.
/// According to the WebAssembly [custom section](https://webassembly.github.io/spec/core/appendix/custom.html)
/// specification, arbitary sets of bytes can be stored in a WebAssembly module without impacting
/// parsers or interpreters. Returns a vector of bytes representing the new WebAssembly module which can
/// be saved to a `.wasm` file
pub fn embed_claims(orig_bytecode: &[u8], claims: &Claims, kp: &KeyPair) -> Result<Vec<u8>> {
    let module: Module = deserialize_buffer(orig_bytecode)?;
    let cleanbytes = serialize(module)?;

    let digest = sha256_digest(cleanbytes.as_slice())?;
    let mut claims = (*claims).clone();
    claims.module_hash = HEXUPPER.encode(digest.as_ref());

    let encoded = claims.encode(&kp)?;
    let encvec = encoded.as_bytes().to_vec();
    let mut m: Module = deserialize_buffer(orig_bytecode)?;
    m.set_custom_section("jwt", encvec);
    let mut buf = Vec::new();
    m.serialize(&mut buf)?;

    Ok(buf)
}

pub fn sign_buffer_with_claims(
    buf: impl AsRef<[u8]>,
    mod_kp: KeyPair,
    acct_kp: KeyPair,
    expires_in_days: Option<u64>,
    not_before_days: Option<u64>,
    caps: Vec<String>,
    tags: Vec<String>,
) -> Result<Vec<u8>> {
    let claims = Claims::with_dates(
        acct_kp.public_key(),
        mod_kp.public_key(),
        Some(caps),
        Some(tags),
        days_from_now_to_jwt_time(not_before_days),
        days_from_now_to_jwt_time(expires_in_days),
    );
    embed_claims(buf.as_ref(), &claims, &acct_kp)
}

fn since_the_epoch() -> std::time::Duration {
    let start = SystemTime::now();
    start
        .duration_since(UNIX_EPOCH)
        .expect("A timey wimey problem has occurred!")
}

fn days_from_now_to_jwt_time(stamp: Option<u64>) -> Option<u64> {
    stamp.map(|e| since_the_epoch().as_secs() + e * SECS_PER_DAY)
}

fn sha256_digest<R: Read>(mut reader: R) -> Result<Digest> {
    let mut context = Context::new(&SHA256);
    let mut buffer = [0; 1024];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        context.update(&buffer[..count]);
    }

    Ok(context.finish())
}

fn compute_hash_without_jwt(module: Module) -> Result<String> {
    let mut refmod = module.clone();
    refmod.clear_custom_section("jwt");
    let modbytes = serialize(refmod)?;

    let digest = sha256_digest(modbytes.as_slice())?;
    Ok(HEXUPPER.encode(digest.as_ref()))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::caps::{KEY_VALUE, MESSAGING};
    use base64::decode;
    use parity_wasm::serialize;

    const WASM_BASE64: &str =
        "AGFzbQEAAAAADAZkeWxpbmuAgMACAAGKgICAAAJgAn9/AX9gAAACwYCAgAAEA2VudgptZW1vcnlCYXNl\
         A38AA2VudgZtZW1vcnkCAIACA2VudgV0YWJsZQFwAAADZW52CXRhYmxlQmFzZQN/AAOEgICAAAMAAQEGi\
         4CAgAACfwFBAAt/AUEACwejgICAAAIKX3RyYW5zZm9ybQAAEl9fcG9zdF9pbnN0YW50aWF0ZQACCYGAgI\
         AAAArpgICAAAPBgICAAAECfwJ/IABBAEoEQEEAIQIFIAAPCwNAIAEgAmoiAywAAEHpAEYEQCADQfkAOgA\
         ACyACQQFqIgIgAEcNAAsgAAsLg4CAgAAAAQuVgICAAAACQCMAJAIjAkGAgMACaiQDEAELCw==";

    #[test]
    fn claims_roundtrip() {
        // Serialize and de-serialize this because the module loader adds bytes to
        // the above base64 encoded module.
        let dec_module = decode(WASM_BASE64).unwrap();
        let m: Module = deserialize_buffer(&dec_module).unwrap();
        let raw_module = serialize(m).unwrap();

        let kp = KeyPair::new_account();
        let claims = Claims {
            module_hash: "".to_string(),
            expires: None,
            id: nuid::next(),
            issued_at: 0,
            issuer: kp.public_key(),
            subject: "test.wasm".to_string(),
            not_before: None,
            tags: None,
            caps: Some(vec![MESSAGING.to_string(), KEY_VALUE.to_string()]),
        };
        let modified_bytecode = embed_claims(&raw_module, &claims, &kp).unwrap();
        println!(
            "Added {} bytes in custom section.",
            modified_bytecode.len() - raw_module.len()
        );
        if let Some(token) = extract_claims(&modified_bytecode).unwrap() {
            assert_eq!(claims.issuer, token.claims.issuer);
            assert_eq!(claims.caps, token.claims.caps);
            assert_ne!(claims.module_hash, token.claims.module_hash);
        } else {
            assert!(false);
        }
    }
}
