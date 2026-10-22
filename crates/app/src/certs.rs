//! Local certificate authority + server certificate.
//!
//! iOS Safari only exposes the camera to secure origins, so the phone loads the page
//! over HTTPS. A small local CA is created once and signs a short-lived server
//! certificate for the PC's LAN addresses. The user can click through Safari's
//! warning, or install and trust the CA on the iPhone once.
//!
//! The CA is name-constrained to private address ranges: should its key ever leak, it
//! still can't be used to impersonate other websites to a phone that trusts it.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints, SanType,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

const CA_NAME: &str = "iPhone Webcam Local CA";
const CA_VALIDITY: Duration = Duration::days(3650);
/// Apple rejects TLS server certificates valid for more than 825 days.
const SERVER_VALIDITY: Duration = Duration::days(397);

/// (network, prefix length) the CA may vouch for.
const PERMITTED_NETWORKS: &[([u8; 4], u8)] = &[
    ([10, 0, 0, 0], 8),
    ([172, 16, 0, 0], 12),
    ([192, 168, 0, 0], 16),
    ([100, 64, 0, 0], 10), // CGNAT / Tailscale
    ([169, 254, 0, 0], 16),
    ([127, 0, 0, 0], 8),
];
const PERMITTED_DNS: &str = "localhost";

pub struct Certificates {
    pub server_cert_pem: Vec<u8>,
    pub server_key_pem: Vec<u8>,
    pub ca_cert_der: Vec<u8>,
    pub ca_cert_path: PathBuf,
}

/// Records what the current server certificate was issued for.
#[derive(Serialize, Deserialize, PartialEq)]
struct ServerMeta {
    ca_sha256: String,
    hosts: Vec<String>,
    not_after_unix: i64,
}

pub fn permitted(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => PERMITTED_NETWORKS.iter().any(|(net, prefix)| {
            let mask = if *prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            u32::from(v4) & mask == u32::from_be_bytes(*net) & mask
        }),
        IpAddr::V6(_) => false,
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn create_ca(dir: &Path) -> Result<()> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, CA_NAME);
    params.distinguished_name = name;
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::minutes(5);
    params.not_after = now + CA_VALIDITY;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    let mut permitted: Vec<GeneralSubtree> = PERMITTED_NETWORKS
        .iter()
        .map(|(net, prefix)| GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix(*net, *prefix)))
        .collect();
    permitted.push(GeneralSubtree::DnsName(PERMITTED_DNS.into()));
    params.name_constraints = Some(NameConstraints { permitted_subtrees: permitted, excluded_subtrees: vec![] });
    let cert = params.self_signed(&key)?;
    fs::write(dir.join("ca.crt"), cert.pem())?;
    fs::write(dir.join("ca.key"), key.serialize_pem())?;
    tracing::info!("Created local CA in {}", dir.display());
    Ok(())
}

fn ca_is_usable(dir: &Path) -> bool {
    let (Ok(cert), Ok(key)) = (fs::read_to_string(dir.join("ca.crt")), fs::read_to_string(dir.join("ca.key"))) else {
        return false;
    };
    let (Ok(key), Ok(der)) = (KeyPair::from_pem(&key), pem_to_der(&cert)) else { return false };
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(&der) else { return false };
    let remaining = parsed.validity().not_after.timestamp() - OffsetDateTime::now_utc().unix_timestamp();
    let constrained = matches!(parsed.name_constraints(), Ok(Some(_)));
    // The key must belong to the certificate, or everything it signs is useless.
    let matches_key = parsed.public_key().subject_public_key.data.as_ref() == key.public_key_raw();
    remaining > Duration::days(30).whole_seconds() && constrained && matches_key
}

/// Make sure a CA and a server certificate covering `ips` exist in `dir`.
///
/// Addresses outside the CA's name constraints (e.g. a public IP) are left out of the
/// certificate - one such name would invalidate the whole certificate. They still
/// work after clicking through Safari's warning.
pub fn ensure(dir: &Path, ips: &[IpAddr]) -> Result<Certificates> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    if !ca_is_usable(dir) {
        create_ca(dir)?;
    }
    let ca_pem = fs::read_to_string(dir.join("ca.crt"))?;
    let ca_key = KeyPair::from_pem(&fs::read_to_string(dir.join("ca.key"))?)?;
    let ca_der = pem_to_der(&ca_pem)?;

    let mut hosts: Vec<String> = ips.iter().filter(|ip| permitted(**ip)).map(|ip| ip.to_string()).collect();
    hosts.push("127.0.0.1".into());
    hosts.push("localhost".into());
    hosts.sort();
    hosts.dedup();

    let meta_path = dir.join("server.json");
    let now = OffsetDateTime::now_utc();
    let current: Option<ServerMeta> =
        fs::read(&meta_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let reuse = current.is_some_and(|m| {
        m.ca_sha256 == sha256_hex(&ca_der)
            && hosts.iter().all(|h| m.hosts.contains(h))
            && m.not_after_unix - now.unix_timestamp() > Duration::days(7).whole_seconds()
    }) && dir.join("server.crt").exists()
        && dir.join("server.key").exists();

    if !reuse {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "iPhone Webcam");
        params.distinguished_name = name;
        params.not_before = now - Duration::minutes(5);
        params.not_after = now + SERVER_VALIDITY;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.subject_alt_names = hosts
            .iter()
            .map(|h| match h.parse::<IpAddr>() {
                Ok(ip) => Ok(SanType::IpAddress(ip)),
                Err(_) => Ok(SanType::DnsName(h.as_str().try_into()?)),
            })
            .collect::<Result<_, rcgen::Error>>()?;
        let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key)?;
        let cert = params.signed_by(&key, &issuer)?;
        fs::write(dir.join("server.crt"), cert.pem())?;
        fs::write(dir.join("server.key"), key.serialize_pem())?;
        let meta = ServerMeta {
            ca_sha256: sha256_hex(&ca_der),
            hosts: hosts.clone(),
            not_after_unix: params.not_after.unix_timestamp(),
        };
        fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;
        tracing::info!("Issued server certificate for {}", hosts.join(", "));
    }

    Ok(Certificates {
        server_cert_pem: fs::read(dir.join("server.crt"))?,
        server_key_pem: fs::read(dir.join("server.key"))?,
        ca_cert_der: ca_der,
        ca_cert_path: dir.join("ca.crt"),
    })
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    Ok(CertificateDer::from_pem_slice(pem.as_bytes()).context("bad PEM certificate")?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn permitted_ranges() {
        assert!(permitted(ip("192.168.1.10")));
        assert!(permitted(ip("10.1.2.3")));
        assert!(permitted(ip("172.31.255.1")));
        assert!(!permitted(ip("172.32.0.1")));
        assert!(!permitted(ip("8.8.8.8")));
    }

    #[test]
    fn certificates_are_reused_and_reissued() {
        let dir = std::env::temp_dir().join(format!("iphone-webcam-test-{}", rand::random::<u64>()));
        let a = ensure(&dir, &[ip("192.168.1.10")]).unwrap();
        let b = ensure(&dir, &[ip("192.168.1.10")]).unwrap();
        assert_eq!(a.server_cert_pem, b.server_cert_pem, "unchanged hosts -> reused");

        let c = ensure(&dir, &[ip("10.0.0.5"), ip("8.8.8.8")]).unwrap();
        assert_ne!(a.server_cert_pem, c.server_cert_pem, "new host -> reissued");
        assert_eq!(a.ca_cert_der, c.ca_cert_der, "CA kept");
        let meta: ServerMeta = serde_json::from_slice(&fs::read(dir.join("server.json")).unwrap()).unwrap();
        assert!(meta.hosts.contains(&"10.0.0.5".to_string()));
        assert!(!meta.hosts.contains(&"8.8.8.8".to_string()), "public IP left out");

        // A new CA must lead to a new server certificate.
        fs::remove_file(dir.join("ca.key")).unwrap();
        let d = ensure(&dir, &[ip("10.0.0.5")]).unwrap();
        assert_ne!(c.ca_cert_der, d.ca_cert_der);
        assert_ne!(c.server_cert_pem, d.server_cert_pem);
        fs::remove_dir_all(&dir).ok();
    }
}

