use anyhow::{Context, Result, bail};
use ring::digest::{Context as DigestContext, SHA1_FOR_LEGACY_USE_ONLY};

pub(crate) const JAVA_TOOL_OPTIONS_ENVIRONMENT: &str = "JAVA_TOOL_OPTIONS";
pub(crate) const JAVA_TRUST_STORE_ENVIRONMENT: &str = "AGORA_SANDBOX_JAVA_TRUST_STORE";
pub(crate) const JAVA_TRUST_STORE_PASSWORD: &str = "changeit";

const JKS_MAGIC: u32 = 0xfeed_feed;
const JKS_VERSION: u32 = 2;
const JKS_TRUSTED_CERTIFICATE: u32 = 2;
const JKS_WHITENER: &[u8] = b"Mighty Aphrodite";
const JAVA_TRUST_STORE_OPTION: &[u8] = b"-Djavax.net.ssl.trustStore=";

pub(crate) fn encode_java_trust_store<'a>(
    certificates: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Vec<u8>> {
    let certificates = certificates.into_iter().collect::<Vec<_>>();
    let count = u32::try_from(certificates.len()).context("too many Java trust certificates")?;
    let mut body = Vec::new();
    write_u32(&mut body, JKS_MAGIC);
    write_u32(&mut body, JKS_VERSION);
    write_u32(&mut body, count);
    for (index, certificate) in certificates.into_iter().enumerate() {
        write_u32(&mut body, JKS_TRUSTED_CERTIFICATE);
        write_utf(&mut body, format!("agora-{index}").as_bytes())?;
        body.extend_from_slice(&0_i64.to_be_bytes());
        write_utf(&mut body, b"X.509")?;
        write_u32(
            &mut body,
            u32::try_from(certificate.len()).context("Java trust certificate is too large")?,
        );
        body.extend_from_slice(certificate);
    }

    let mut digest = DigestContext::new(&SHA1_FOR_LEGACY_USE_ONLY);
    for character in JAVA_TRUST_STORE_PASSWORD.encode_utf16() {
        digest.update(&character.to_be_bytes());
    }
    digest.update(JKS_WHITENER);
    digest.update(&body);
    body.extend_from_slice(digest.finish().as_ref());
    Ok(body)
}

pub(crate) fn merged_java_tool_options(
    existing: Option<&[u8]>,
    inherited_managed_store: Option<&[u8]>,
    managed_store: &[u8],
) -> Vec<u8> {
    let existing = existing.unwrap_or_default();
    if let Some(configured) = configured_java_trust_store(existing)
        && inherited_managed_store != Some(configured)
    {
        return existing.to_vec();
    }
    if inherited_managed_store == Some(managed_store)
        && configured_java_trust_store(existing) == Some(managed_store)
    {
        return existing.to_vec();
    }

    let mut options = existing.to_vec();
    if !options.is_empty() && !options.last().is_some_and(u8::is_ascii_whitespace) {
        options.push(b' ');
    }
    options.extend_from_slice(JAVA_TRUST_STORE_OPTION);
    append_java_option_value(&mut options, managed_store);
    options.extend_from_slice(b" -Djavax.net.ssl.trustStoreType=JKS");
    options.extend_from_slice(b" -Djavax.net.ssl.trustStorePassword=");
    options.extend_from_slice(JAVA_TRUST_STORE_PASSWORD.as_bytes());
    options
}

fn append_java_option_value(output: &mut Vec<u8>, value: &[u8]) {
    let quote = if value.iter().any(u8::is_ascii_whitespace) {
        if !value.contains(&b'"') {
            Some(b'"')
        } else if !value.contains(&b'\'') {
            Some(b'\'')
        } else {
            None
        }
    } else {
        None
    };
    if let Some(quote) = quote {
        output.push(quote);
        output.extend_from_slice(value);
        output.push(quote);
    } else {
        output.extend_from_slice(value);
    }
}

fn configured_java_trust_store(options: &[u8]) -> Option<&[u8]> {
    let start = options
        .windows(JAVA_TRUST_STORE_OPTION.len())
        .rposition(|window| window == JAVA_TRUST_STORE_OPTION)?
        + JAVA_TRUST_STORE_OPTION.len();
    let value = &options[start..];
    let (value, terminator) = match value.first().copied() {
        Some(quote @ (b'\'' | b'"')) => (&value[1..], Some(quote)),
        _ => (value, None),
    };
    let end = value
        .iter()
        .position(|byte| match terminator {
            Some(quote) => *byte == quote,
            None => byte.is_ascii_whitespace() || matches!(byte, b'\'' | b'"'),
        })
        .unwrap_or(value.len());
    Some(&value[..end])
}

fn write_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn write_utf(output: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    if value.contains(&0) {
        bail!("Java trust store string contains a null byte");
    }
    let length = u16::try_from(value.len()).context("Java trust store string is too long")?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}
