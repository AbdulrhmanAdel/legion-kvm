//! Reproduce a verified repair in a session copy; never edit installed Proton.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    schema: u32,
    source_sha256: String,
    unix_sha256: String,
    target_sha256: String,
    target_bytes: usize,
    pieces: Vec<Piece>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Piece {
    Copy { source: usize, length: usize },
    Literal { data: String },
}

pub(crate) fn prepare(
    recipe: &Path,
    proton: &Path,
    destination: &Path,
) -> Result<serde_json::Value> {
    ensure!(
        fs::metadata(recipe)?.len() <= 32 * 1024 * 1024,
        "Wine recipe is too large"
    );
    let recipe: Recipe = serde_json::from_slice(&fs::read(recipe)?)?;
    ensure!(
        recipe.schema == 1 && recipe.target_bytes <= 32 * 1024 * 1024,
        "invalid Wine recipe"
    );
    let source = fs::read(proton.join("files/lib/wine/x86_64-windows/ntdll.dll"))?;
    let unix = fs::read(proton.join("files/lib/wine/x86_64-unix/ntdll.so"))?;
    ensure!(
        format!("{:x}", Sha256::digest(&source)) == recipe.source_sha256
            && format!("{:x}", Sha256::digest(&unix)) == recipe.unix_sha256,
        "selected Proton modules do not match the compatibility recipe"
    );
    let bytes = apply(&recipe, &source)?;
    fs::create_dir_all(
        destination
            .parent()
            .context("repair destination has no parent")?,
    )?;
    fs::write(destination, bytes)?;
    Ok(
        serde_json::json!({"stock_pe_sha256":recipe.source_sha256,"stock_unix_sha256":recipe.unix_sha256,
        "replacement_sha256":recipe.target_sha256,"migrate_loader":true}),
    )
}

fn apply(recipe: &Recipe, source: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(recipe.target_bytes);
    for piece in &recipe.pieces {
        match piece {
            Piece::Copy {
                source: start,
                length,
            } => {
                let end = start
                    .checked_add(*length)
                    .context("repair offset overflow")?;
                output.extend_from_slice(
                    source
                        .get(*start..end)
                        .context("repair copy outside source")?,
                );
            }
            Piece::Literal { data } => {
                ensure!(data.len() % 2 == 0, "invalid repair literal");
                for bytes in data.as_bytes().as_chunks::<2>().0 {
                    output.push(u8::from_str_radix(std::str::from_utf8(bytes)?, 16)?);
                }
            }
        }
        ensure!(
            output.len() <= recipe.target_bytes,
            "repair exceeds declared size"
        );
    }
    ensure!(
        output.len() == recipe.target_bytes
            && format!("{:x}", Sha256::digest(&output)) == recipe.target_sha256,
        "Wine repair checksum failed"
    );
    Ok(output)
}

#[cfg(test)]
#[path = "../tests/unit/wine_recipe.rs"]
mod tests;
