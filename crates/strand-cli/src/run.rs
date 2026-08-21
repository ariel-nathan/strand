//! Executes a compiled Strand program and renders its result.
//!
//! Decoding is driven by the typed IR, so this doubles as a readable check on
//! the design doc: if a record's fields print correctly, their offsets are
//! right; if `Ok(2.5)` prints as `Ok(2.5)`, the payload slot round-tripped.

use anyhow::{anyhow, Result};
use strandc::layout::wasm_arity;
use strandc::hir::{Hir, Ty};
// wasmtime 48 has its own error type, so its `Context` is the one that applies
// here; `?` then converts into `anyhow::Error`.
use wasmtime::error::Context as _;
use wasmtime::{Engine, Instance, Store, Val};

/// Every value occupies whole 8-byte slots (`docs/strand-design.md`).
const WORD: usize = 8;

/// Compiles nothing — takes an already-emitted module — and calls `main`.
pub fn run_main(hir: &Hir, wasm: &[u8]) -> Result<String> {
    let main = hir
        .funcs
        .iter()
        .find(|f| f.name == "main")
        .ok_or_else(|| anyhow!("this program has no `main` function"))?;
    if main.param_count != 0 {
        return Err(anyhow!("`main` must take no parameters"));
    }

    let engine = Engine::default();
    let module = wasmtime::Module::new(&engine, wasm).context("wasmtime rejected the module")?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).context("instantiation failed")?;

    let func = instance
        .get_func(&mut store, "main")
        .ok_or_else(|| anyhow!("`main` is not exported"))?;
    let mut results = vec![Val::I32(0); wasm_arity(&main.ret)];
    func.call(&mut store, &[], &mut results).context("trap while running `main`")?;

    let bytes = match instance.get_memory(&mut store, "memory") {
        Some(memory) => memory.data(&store).to_vec(),
        None => Vec::new(),
    };

    Ok(render(hir, &main.ret, &results, &bytes))
}

/// Renders a returned value using its static type.
fn render(hir: &Hir, ty: &Ty, values: &[Val], memory: &[u8]) -> String {
    match ty {
        Ty::Unit => "()".to_string(),
        // A node was drawn, not returned: the frame's array holds it, and
        // `strand view` is what reads that.
        Ty::Node => "<node>".to_string(),
        Ty::Int => values[0].unwrap_i64().to_string(),
        Ty::Float => format_float(values[0].unwrap_f64()),
        Ty::Bool => (values[0].unwrap_i32() != 0).to_string(),
        Ty::Str => render_string(values[0].unwrap_i32() as usize, memory),
        Ty::Record(_) | Ty::Sum(_) | Ty::List(_) => {
            render_pointer(hir, ty, values[0].unwrap_i32() as usize, memory)
        }
        Ty::Option(inner) => {
            let (tag, payload) = (values[0].unwrap_i32(), values[1].unwrap_i64());
            if tag == 0 {
                format!("Some({})", render_payload(hir, inner, payload, memory))
            } else {
                "None".to_string()
            }
        }
        Ty::Result(ok, err) => {
            let (tag, payload) = (values[0].unwrap_i32(), values[1].unwrap_i64());
            if tag == 0 {
                format!("Ok({})", render_payload(hir, ok, payload, memory))
            } else {
                format!("Err({})", render_payload(hir, err, payload, memory))
            }
        }
        Ty::Never | Ty::Error => "<unknown>".to_string(),
    }
}

/// Widens the single payload slot back into a value of `ty` (§6.2).
fn render_payload(hir: &Hir, ty: &Ty, payload: i64, memory: &[u8]) -> String {
    let value = match ty {
        Ty::Unit => return "()".to_string(),
        Ty::Int => Val::I64(payload),
        Ty::Float => Val::F64(payload as u64),
        Ty::Bool => Val::I32(payload as i32),
        _ => Val::I32(payload as i32),
    };
    render(hir, ty, &[value], memory)
}

fn render_string(ptr: usize, memory: &[u8]) -> String {
    let Some(len) = read_u32(memory, ptr) else {
        return "<bad string pointer>".to_string();
    };
    let start = ptr + 4;
    let end = start + len as usize;
    match memory.get(start..end) {
        Some(bytes) => format!("{:?}", String::from_utf8_lossy(bytes)),
        None => "<bad string>".to_string(),
    }
}

fn render_pointer(hir: &Hir, ty: &Ty, ptr: usize, memory: &[u8]) -> String {
    match ty {
        Ty::Record(id) => {
            let def = &hir.records[id.0 as usize];
            let mut parts = Vec::new();
            let mut offset = 0usize;
            for (name, field_ty) in &def.fields {
                let value = read_field(hir, field_ty, ptr + offset, memory);
                parts.push(format!("{name}: {value}"));
                offset += wasm_arity(field_ty).max(1) * WORD;
            }
            format!("{} {{ {} }}", def.name, parts.join(", "))
        }
        Ty::Sum(id) => {
            let def = &hir.sums[id.0 as usize];
            let niladic = def.variants.iter().all(|v| v.fields.is_empty());
            // All-niladic sums are the tag itself, not a pointer (§3).
            let index = if niladic { ptr as u32 } else { read_u32(memory, ptr).unwrap_or(0) };
            let Some(variant) = def.variants.get(index as usize) else {
                return "<bad variant>".to_string();
            };
            if variant.fields.is_empty() {
                return variant.name.clone();
            }
            let mut parts = Vec::new();
            let mut offset = WORD;
            for (name, field_ty) in &variant.fields {
                let value = read_field(hir, field_ty, ptr + offset, memory);
                parts.push(format!("{name}: {value}"));
                offset += wasm_arity(field_ty).max(1) * WORD;
            }
            format!("{}({})", variant.name, parts.join(", "))
        }
        _ => format!("<{ptr:#x}>"),
    }
}

fn read_field(hir: &Hir, ty: &Ty, at: usize, memory: &[u8]) -> String {
    match ty {
        // Unrepresentable as a field: the checker rejects a record that holds
        // one, so this arm exists only to keep the match total.
        Ty::Node => "<node>".to_string(),
        Ty::Int => read_i64(memory, at).map_or("<oob>".into(), |v| v.to_string()),
        Ty::Float => read_i64(memory, at)
            .map_or("<oob>".into(), |v| format_float(f64::from_bits(v as u64))),
        Ty::Bool => read_u32(memory, at).map_or("<oob>".into(), |v| (v != 0).to_string()),
        Ty::Str => read_u32(memory, at)
            .map_or("<oob>".into(), |ptr| render_string(ptr as usize, memory)),
        Ty::Record(_) | Ty::Sum(_) | Ty::List(_) => read_u32(memory, at)
            .map_or("<oob>".into(), |ptr| render_pointer(hir, ty, ptr as usize, memory)),
        Ty::Option(inner) => match (read_u32(memory, at), read_i64(memory, at + WORD)) {
            (Some(0), Some(payload)) => {
                format!("Some({})", render_payload(hir, inner, payload, memory))
            }
            (Some(_), _) => "None".to_string(),
            _ => "<oob>".to_string(),
        },
        Ty::Result(ok, err) => match (read_u32(memory, at), read_i64(memory, at + WORD)) {
            (Some(0), Some(payload)) => {
                format!("Ok({})", render_payload(hir, ok, payload, memory))
            }
            (Some(_), Some(payload)) => {
                format!("Err({})", render_payload(hir, err, payload, memory))
            }
            _ => "<oob>".to_string(),
        },
        Ty::Unit => "()".to_string(),
        Ty::Never | Ty::Error => "<unknown>".to_string(),
    }
}

/// Prints floats so whole numbers keep a decimal point, making `2.0` and `2`
/// distinguishable in golden files.
fn format_float(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

fn read_u32(memory: &[u8], at: usize) -> Option<u32> {
    let bytes = memory.get(at..at + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_i64(memory: &[u8], at: usize) -> Option<i64> {
    let bytes = memory.get(at..at + 8)?;
    Some(i64::from_le_bytes(bytes.try_into().ok()?))
}
