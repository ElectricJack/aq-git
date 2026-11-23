use super::*;
pub(super) fn validate_manifest(m: &Manifest, limits: &ArtifactLimits) -> Result<()> {
    if m.format_version != 1 {
        return Err(Error::new(
            "unsupported_feature",
            "unsupported artifact format",
        ));
    }
    operations::id(&m.repository_id)?;
    operations::id(&m.transfer_id)?;
    operations::id(&m.producer_installation_id)?;
    if m.object_format != "sha1" {
        return Err(Error::new(
            "unsupported_feature",
            "artifact object format unsupported",
        ));
    }
    validate_manifest_objects(m)?;
    validate_manifest_bundle(m, limits)
}
fn validate_manifest_objects(m: &Manifest) -> Result<()> {
    operations::oid(&m.base)?;
    operations::oid(&m.source)?;
    if m.prerequisites.len() > 64 {
        return Err(Error::new(
            "limit_exceeded",
            "too many artifact prerequisites",
        ));
    }
    let mut seen = BTreeSet::new();
    for p in &m.prerequisites {
        operations::oid(p)?;
        if !seen.insert(p.hex.clone()) {
            return Err(Error::new("invalid_artifact", "duplicate prerequisite"));
        }
    }
    Ok(())
}
fn validate_manifest_bundle(m: &Manifest, limits: &ArtifactLimits) -> Result<()> {
    if m.bundle_size > limits.max_bundle_bytes {
        return Err(Error::new(
            "limit_exceeded",
            "artifact exceeds compressed byte limit",
        ));
    }
    if m.reference_only {
        if m.bundle_size != 0 || m.bundle_sha256.is_some() {
            return Err(Error::new(
                "invalid_artifact",
                "reference-only manifest contains bundle metadata",
            ));
        }
    } else if m.bundle_size == 0 || !m.bundle_sha256.as_deref().is_some_and(valid_sha256) {
        return Err(Error::new("invalid_artifact", "invalid bundle size/digest"));
    }
    Ok(())
}
pub(super) fn bundle_header(path: &Path) -> Result<(BTreeSet<String>, String, String)> {
    let mut reader = BufReader::new(open_file(path)?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line != "# v2 git bundle\n" {
        return Err(Error::new(
            "unsupported_feature",
            "only complete SHA-1 v2 bundles are accepted",
        ));
    }
    let mut prerequisites = BTreeSet::new();
    let mut refs = Vec::new();
    let mut bytes = line.len();
    loop {
        line.clear();
        let n = reader.by_ref().take(16385).read_line(&mut line)?;
        bytes += n;
        if !(1..=16384).contains(&n) || bytes > 1024 * 1024 {
            return Err(Error::new(
                "invalid_artifact",
                "invalid bounded bundle header",
            ));
        }
        if line == "\n" {
            break;
        }
        parse_header_line(line.trim_end_matches('\n'), &mut prerequisites, &mut refs)?;
    }
    if refs.len() != 1 {
        return Err(Error::new(
            "invalid_artifact",
            "bundle must advertise exactly one transfer ref",
        ));
    }
    let (oid, name) = refs.pop().unwrap();
    Ok((prerequisites, oid, name))
}
fn parse_header_line(
    line: &str,
    prerequisites: &mut BTreeSet<String>,
    refs: &mut Vec<(String, String)>,
) -> Result<()> {
    if let Some(p) = line.strip_prefix('-') {
        let oid = p.split_once(' ').map(|x| x.0).unwrap_or(p);
        operations::oid(&ObjectId {
            algorithm: "sha1".into(),
            hex: oid.into(),
        })?;
        if !prerequisites.insert(oid.into()) {
            return Err(Error::new(
                "invalid_artifact",
                "duplicate bundle prerequisite",
            ));
        }
    } else {
        let Some((oid, name)) = line.split_once(' ') else {
            return Err(Error::new("invalid_artifact", "malformed advertised ref"));
        };
        operations::oid(&ObjectId {
            algorithm: "sha1".into(),
            hex: oid.into(),
        })?;
        refs.push((oid.to_owned(), name.to_owned()));
    }
    Ok(())
}

fn valid_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
}
