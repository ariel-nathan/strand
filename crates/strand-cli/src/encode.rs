//! Encodes a typed message into the bytes an actor's channel carries.
//!
//! The layout is the design doc's, and deliberately nothing more: because the
//! checker guarantees message types are flat, the encoded bytes are already a
//! valid value in the receiving arena. Nothing decodes on arrival.
//!
//! This is the sending half of a typed channel. Both ends read the layout from
//! the same `Hir`, so a mismatch is a compile error rather than a corrupt read.

use anyhow::{anyhow, bail, Result};
use strandc::hir::{Hir, Ty};

/// Every value occupies whole 8-byte slots (`docs/strand-design.md`).
const WORD: usize = 8;

/// A message to send, as written on a command line: `Inc`, or `Add 5`.
pub fn encode(hir: &Hir, message_ty: &Ty, spec: &str) -> Result<Vec<u8>> {
    let mut words = spec.split_whitespace();
    let head = words.next().ok_or_else(|| anyhow!("empty message"))?;
    let args: Vec<&str> = words.collect();

    match message_ty {
        Ty::Str => Ok(spec.as_bytes().to_vec()),
        Ty::Int => Ok(parse_int(head)?.to_le_bytes().to_vec()),
        Ty::Bool => {
            let value: bool = head.parse().map_err(|_| anyhow!("expected true or false"))?;
            Ok((value as i32).to_le_bytes().to_vec())
        }
        Ty::Float => {
            let value: f64 = head.parse().map_err(|_| anyhow!("expected a float"))?;
            Ok(value.to_le_bytes().to_vec())
        }
        Ty::Sum(id) => encode_variant(hir, id.0 as usize, head, &args),
        other => bail!("cannot encode a message of type {}", hir.ty(other)),
    }
}

fn encode_variant(hir: &Hir, sum: usize, name: &str, args: &[&str]) -> Result<Vec<u8>> {
    let def = &hir.sums[sum];
    let index = def
        .variants
        .iter()
        .position(|v| v.name == name)
        .ok_or_else(|| {
            let known: Vec<&str> = def.variants.iter().map(|v| v.name.as_str()).collect();
            anyhow!("`{name}` is not a variant of `{}`; expected one of {}", def.name, known.join(", "))
        })?;
    let variant = &def.variants[index];

    if args.len() != variant.fields.len() {
        bail!(
            "`{}` takes {} argument(s), found {}",
            variant.name,
            variant.fields.len(),
            args.len()
        );
    }

    // All-niladic sums are the bare tag, not a pointer to one (§6.3).
    if def.variants.iter().all(|v| v.fields.is_empty()) {
        return Ok((index as i32).to_le_bytes().to_vec());
    }

    // Boxed: the tag in the first slot, then one slot per field.
    let mut bytes = vec![0u8; WORD * (1 + variant.fields.len())];
    bytes[..4].copy_from_slice(&(index as i32).to_le_bytes());

    for (position, ((field, ty), raw)) in variant.fields.iter().zip(args).enumerate() {
        let at = WORD * (1 + position);
        match ty {
            Ty::Int => bytes[at..at + 8].copy_from_slice(&parse_int(raw)?.to_le_bytes()),
            Ty::Float => {
                let value: f64 =
                    raw.parse().map_err(|_| anyhow!("field `{field}` expects a float"))?;
                bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
            }
            Ty::Bool => {
                let value: bool =
                    raw.parse().map_err(|_| anyhow!("field `{field}` expects true or false"))?;
                bytes[at..at + 4].copy_from_slice(&(value as i32).to_le_bytes());
            }
            // The checker rejects non-flat fields, so this is unreachable in
            // practice; refusing beats writing a pointer another arena cannot use.
            other => bail!("field `{field}` is {}, which cannot cross a channel", hir.ty(other)),
        }
    }
    Ok(bytes)
}

fn parse_int(raw: &str) -> Result<i64> {
    raw.parse::<i64>().map_err(|_| anyhow!("`{raw}` is not an int"))
}
