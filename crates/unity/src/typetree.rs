//! Turns an object's bytes into JSON by following its type tree.
//!
//! The tree (see [`crate::serialized`]) lists the fields in the order they are stored; this walker reads them one
//! after the other. The result has the shape of UnityPy's `read_typetree()`, because the rest of the tooling was
//! written against dumps made with it:
//!
//! * a structure is an object keyed by field name, so a reference is `{"m_FileID": 0, "m_PathID": 123}`;
//! * a `vector`/`map`/`set` (a node whose only child is an `Array`: an i32 count, then the elements) is an array,
//!   and a `pair` is a two-element array, so a map is an array of `[key, value]`;
//! * `bool` is true/false, but a C# bool field of a script is serialized as `UInt8` and comes out as 0/1;
//! * a float is widened to a double exactly, the way Python holds it;
//! * a `string` is an i32 length and UTF-8 bytes, padded to 4;
//! * raw bytes (`TypelessData`) have no JSON form. The existing dumps were written with `json.dumps(default=str)`,
//!   which stores Python's `repr` of the bytes (`"b'\\x00\\x01'"`), and that text is reproduced.
//!
//! Padding: a node flagged "aligned" is followed by padding to a multiple of 4, counted from the start of the object.
//!
//! Managed references ([SerializeReference] fields): the object ends with a `references` structure of type
//! `ManagedReferencesRegistry`: `version`, then `RefIds`, an array of `{rid, type: {class, ns, asm}, data}`. The
//! layout of `data` is not in the object's own tree: it is the *ref type* of the file whose class, namespace and
//! assembly match `type`. Fields elsewhere in the object refer to an entry by its `rid`.
//!
//! Two quirks of UnityPy are kept on purpose, so that an object that fails there fails here too and the two dumps
//! hold the same set of objects: an array whose element type is flagged "aligned" is padded once after the last
//! element rather than after each, and a registry nested inside another registry's data is left out.

use std::ops::Range;

use serde_json::{Map, Number, Value};

use crate::bytes::Cursor;
use crate::serialized::{children, Kind, SerializedType, TypeNode, TypeTree};

/// A `TypelessData` field met while reading: its field name and where its bytes are in the object.
pub type Blob = (String, Range<usize>);

/// Reads a whole object. It is an error if the tree does not consume exactly the object's bytes: that means the
/// tree and the data disagree and nothing read can be trusted.
pub fn read(tree: &TypeTree, data: &[u8], ref_types: &[SerializedType]) -> Result<Value, String> {
    let mut walker = Walker { r: Cursor::new(data, false), ref_types, blobs: None };
    let value = walker.value(&tree.nodes, 0, false)?;
    if walker.r.pos != data.len() {
        return Err(format!("type tree read {} of {} bytes", walker.r.pos, data.len()));
    }
    Ok(value)
}

/// Like [`read`], but raw byte fields come back as ranges of `data` (and as null in the JSON) instead of text:
/// for the objects whose bytes are the point, such as inline pixels and vertex buffers.
pub fn read_with_blobs(
    tree: &TypeTree,
    data: &[u8],
    ref_types: &[SerializedType],
) -> Result<(Value, Vec<Blob>), String> {
    let mut blobs = Vec::new();
    let mut walker = Walker { r: Cursor::new(data, false), ref_types, blobs: Some(&mut blobs) };
    let value = walker.value(&tree.nodes, 0, false)?;
    if walker.r.pos != data.len() {
        return Err(format!("type tree read {} of {} bytes", walker.r.pos, data.len()));
    }
    Ok((value, blobs))
}

struct Walker<'a> {
    r: Cursor<'a>,
    ref_types: &'a [SerializedType],
    blobs: Option<&'a mut Vec<Blob>>,
}

impl Walker<'_> {
    fn value(&mut self, nodes: &[TypeNode], i: usize, in_registry: bool) -> Result<Value, String> {
        let node = &nodes[i];
        let mut align = node.aligned;
        let value = match node.kind {
            Kind::Pair => {
                let mut pair = Vec::with_capacity(2);
                for child in children(nodes, i).take(2) {
                    pair.push(self.value(nodes, child, in_registry)?);
                }
                Value::Array(pair)
            }
            Kind::ReferencedObject => Value::Object(self.referenced_object(nodes, i, in_registry, true)?),
            Kind::Class | Kind::Array | Kind::ReferencedObjectData | Kind::ManagedReferencesRegistry => {
                match children(nodes, i).next() {
                    Some(array) if nodes[array].kind == Kind::Array => {
                        align |= nodes[array].aligned;
                        let count = self.count()?;
                        let element = children(nodes, array).nth(1).ok_or("array node without an element type")?;
                        if nodes[element].aligned {
                            self.array(nodes, element, count, in_registry)?
                        } else {
                            let mut items = Vec::with_capacity(count.min(4096));
                            for _ in 0..count {
                                items.push(self.value(nodes, element, in_registry)?);
                            }
                            Value::Array(items)
                        }
                    }
                    _ => Value::Object(self.fields(nodes, i, in_registry, true)?),
                }
            }
            _ => self.scalar(node)?,
        };
        if align {
            self.r.align(4);
        }
        Ok(value)
    }

    /// `count` values of an element type that carries the alignment flag: padded once, after the last one.
    fn array(&mut self, nodes: &[TypeNode], i: usize, count: usize, in_registry: bool) -> Result<Value, String> {
        let node = &nodes[i];
        let mut align = node.aligned;
        let mut items = Vec::with_capacity(count.min(4096));
        match node.kind {
            Kind::Pair => {
                let mut pair = children(nodes, i);
                let (first, second) = (pair.next().ok_or("pair without members")?, pair.next().ok_or("pair without members")?);
                for _ in 0..count {
                    let key = self.value(nodes, first, in_registry)?;
                    items.push(Value::Array(vec![key, self.value(nodes, second, in_registry)?]));
                }
            }
            Kind::ReferencedObject => {
                for _ in 0..count {
                    items.push(Value::Object(self.referenced_object(nodes, i, in_registry, false)?));
                }
            }
            Kind::Class | Kind::Array | Kind::ReferencedObjectData | Kind::ManagedReferencesRegistry => {
                match children(nodes, i).next() {
                    Some(array) if nodes[array].kind == Kind::Array => {
                        align |= nodes[array].aligned;
                        let element = children(nodes, array).nth(1).ok_or("array node without an element type")?;
                        for _ in 0..count {
                            let inner = self.count()?;
                            if nodes[element].aligned {
                                items.push(self.array(nodes, element, inner, in_registry)?);
                            } else {
                                let mut row = Vec::with_capacity(inner.min(4096));
                                for _ in 0..inner {
                                    row.push(self.value(nodes, element, in_registry)?);
                                }
                                items.push(Value::Array(row));
                            }
                        }
                    }
                    _ => {
                        for _ in 0..count {
                            items.push(Value::Object(self.fields(nodes, i, in_registry, false)?));
                        }
                    }
                }
            }
            _ => {
                for _ in 0..count {
                    items.push(self.scalar(node)?);
                }
            }
        }
        if align {
            self.r.align(4);
        }
        Ok(Value::Array(items))
    }

    /// The members of a structure. `skip_nested_registry`: a second registry below one already being read is not
    /// read at all (and so not consumed), as UnityPy does for a structure outside an aligned array.
    fn fields(
        &mut self,
        nodes: &[TypeNode],
        i: usize,
        mut in_registry: bool,
        skip_nested_registry: bool,
    ) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        for child in children(nodes, i) {
            if skip_nested_registry && nodes[child].kind == Kind::ManagedReferencesRegistry {
                if in_registry {
                    continue;
                }
                in_registry = true;
            }
            out.insert(nodes[child].name.clone(), self.value(nodes, child, in_registry)?);
        }
        Ok(out)
    }

    /// `{rid, type, data}`: `data` is read with the tree of the ref type that `type` names. An entry with an empty
    /// class name is the null reference and has no data.
    fn referenced_object(
        &mut self,
        nodes: &[TypeNode],
        i: usize,
        in_registry: bool,
        null_allowed: bool,
    ) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        for child in children(nodes, i) {
            if nodes[child].kind != Kind::ReferencedObjectData {
                out.insert(nodes[child].name.clone(), self.value(nodes, child, in_registry)?);
                continue;
            }
            let part = |key: &str| out.get("type").and_then(|t| t.get(key)).and_then(Value::as_str).unwrap_or("");
            let (class, namespace, assembly) = (part("class"), part("ns"), part("asm"));
            if class.is_empty() {
                if null_allowed {
                    continue;
                }
                return Err("null managed reference inside an aligned array".into());
            }
            let ref_types = self.ref_types;
            let tree = ref_types
                .iter()
                .find(|t| t.class_name == class && t.namespace == namespace && t.assembly == assembly)
                .and_then(|t| t.tree.as_ref())
                .ok_or_else(|| format!("no type tree for the managed reference type {} {} {}", class, namespace, assembly))?;
            let data = self.value(&tree.nodes, 0, in_registry)?;
            out.insert(nodes[child].name.clone(), data);
        }
        Ok(out)
    }

    fn count(&mut self) -> Result<usize, String> {
        let count = self.r.i32()?;
        if count < 0 {
            return Err("negative array length".into());
        }
        // Each element takes at least... nothing (an empty structure), so the count cannot be checked against the
        // bytes left; a corrupt one runs into the end-of-data error of the first element instead.
        Ok(count as usize)
    }

    fn scalar(&mut self, node: &TypeNode) -> Result<Value, String> {
        let r = &mut self.r;
        Ok(match node.kind {
            Kind::I8 => Value::from(r.u8()? as i8),
            Kind::U8 => Value::from(r.u8()?),
            Kind::I16 => Value::from(r.i16()?),
            Kind::U16 => Value::from(r.u16()?),
            Kind::I32 => Value::from(r.i32()?),
            Kind::U32 => Value::from(r.u32()?),
            Kind::I64 => Value::from(r.i64()?),
            Kind::U64 => Value::from(r.u64()?),
            // JSON has no NaN or infinity; those become null.
            Kind::F32 => Number::from_f64(r.f32()? as f64).map_or(Value::Null, Value::Number),
            Kind::F64 => Number::from_f64(r.f64()?).map_or(Value::Null, Value::Number),
            Kind::Bool => Value::Bool(r.u8()? != 0),
            Kind::Str => {
                let len = r.i32()?;
                if len > 0 && len as usize <= r.remaining() {
                    let text = String::from_utf8_lossy(r.bytes(len as usize)?).into_owned();
                    r.align(4);
                    Value::String(text)
                } else {
                    Value::String(String::new())
                }
            }
            Kind::Typeless => {
                let len = r.i32()?;
                if len < 0 {
                    return Err("negative byte array length".into());
                }
                let start = r.pos;
                let bytes = r.bytes(len as usize)?;
                match self.blobs.as_mut() {
                    Some(blobs) => {
                        blobs.push((node.name.clone(), start..start + len as usize));
                        Value::Null
                    }
                    None => Value::String(python_bytes_repr(bytes)),
                }
            }
            _ => return Err(format!("type {} cannot be read as a value", node.type_name)),
        })
    }
}

/// What Python prints for a `bytes` object: `b'...'` with printable ASCII as is and the rest escaped.
pub fn python_bytes_repr(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = String::with_capacity(bytes.len() + 3);
    out.push('b');
    out.push(quote as char);
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if b == quote => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\x");
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
        }
    }
    out.push(quote as char);
    out
}

#[cfg(test)]
mod tests {
    use super::python_bytes_repr;

    #[test]
    fn bytes_are_written_as_python_prints_them() {
        // NUL, 1, apostrophe, quote, backslash, tab, line feed, carriage return, 'A', 200, 255
        let bytes = [0, 1, 39, 34, 92, 9, 10, 13, 65, 200, 255];
        assert_eq!(python_bytes_repr(&bytes), r#"b'\x00\x01\'"\\\t\n\rA\xc8\xff'"#);
        assert_eq!(python_bytes_repr(b"it's"), r#"b"it's""#);
        assert_eq!(python_bytes_repr(b""), "b''");
    }
}
