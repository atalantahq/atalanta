use std::collections::HashMap;

/// Parsed Android Binary XML document.
///
/// Android compiles XML resources into a binary format (AXML) in the APK.
/// This parser extracts the XML structure from the binary format.
#[derive(Debug, Clone)]
pub struct AxmlDocument {
    pub root: Option<AxmlElement>,
}

/// An element in the parsed AXML tree.
#[derive(Debug, Clone)]
pub struct AxmlElement {
    pub name: String,
    pub namespace: Option<String>,
    pub attributes: HashMap<String, String>,
    pub children: Vec<AxmlElement>,
}

impl AxmlDocument {
    /// Get an attribute value from a named element (searches root).
    pub fn get_attribute(&self, element_name: &str, attr_name: &str) -> Option<String> {
        self.root
            .as_ref()
            .and_then(|root| root.get_attribute(element_name, attr_name))
    }
}

impl AxmlElement {
    /// Get an attribute value from this element or its children.
    pub fn get_attribute(&self, element_name: &str, attr_name: &str) -> Option<String> {
        if self.name == element_name {
            return self.attributes.get(attr_name).cloned();
        }
        for child in &self.children {
            if let Some(val) = child.get_attribute(element_name, attr_name) {
                return Some(val);
            }
        }
        None
    }
}

/// Binary XML chunk types.
const CHUNK_AXML: u16 = 0x0003;
const CHUNK_STRING_POOL: u16 = 0x0001;
const CHUNK_RESOURCE_MAP: u16 = 0x0180;
const CHUNK_START_NAMESPACE: u16 = 0x0100;
const CHUNK_END_NAMESPACE: u16 = 0x0101;
const CHUNK_START_TAG: u16 = 0x0102;
const CHUNK_END_TAG: u16 = 0x0103;

/// Deeper trees are refused rather than recursed into.
const MAX_DEPTH: usize = 512;
/// Cap on the bytes a string pool decodes to (see `parse_string_pool`).
const MAX_POOL_DECODED_BYTES: usize = 64 << 20;

/// `TypedValue.complexToFloat`: 24-bit signed mantissa with a 2-bit radix.
fn complex_to_float(complex: u32) -> f32 {
    const RADIX: [f32; 4] = [
        1.0 / 256.0,
        1.0 / 32_768.0,
        1.0 / 8_388_608.0,
        1.0 / 2_147_483_648.0,
    ];
    ((complex & 0xFFFF_FF00) as i32) as f32 * RADIX[((complex >> 4) & 3) as usize]
}

/// `android:` attribute names by resource id (from the framework's
/// `public.xml`) for the attributes the style mapper and inflater read.
fn framework_attr_name(id: u32) -> Option<&'static str> {
    Some(match id {
        0x0101000e => "enabled",
        0x01010095 => "textSize",
        0x01010097 => "textStyle",
        0x01010098 => "textColor",
        0x010100af => "gravity",
        0x010100b3 => "layout_gravity",
        0x010100c4 => "orientation",
        0x010100d0 => "id",
        0x010100d4 => "background",
        0x010100d5 => "padding",
        0x010100d6 => "paddingLeft",
        0x010100d7 => "paddingTop",
        0x010100d8 => "paddingRight",
        0x010100d9 => "paddingBottom",
        0x010100dc => "visibility",
        0x010100f4 => "layout_width",
        0x010100f5 => "layout_height",
        0x010100f6 => "layout_margin",
        0x010100f7 => "layout_marginLeft",
        0x010100f8 => "layout_marginTop",
        0x010100f9 => "layout_marginRight",
        0x010100fa => "layout_marginBottom",
        0x0101014f => "text",
        0x01010150 => "hint",
        0x0101015c => "password",
        0x01010181 => "layout_weight",
        0x01010220 => "inputType",
        0x01010273 => "contentDescription",
        0x0101031f => "alpha",
        _ => return None,
    })
}

/// Symbolic names for integer-valued framework enums and flags, as aapt's
/// source XML spelled them (apktool `ResEnumAttr`/`ResFlagsAttr`).
fn symbolic_value(attr: &str, data: u32) -> Option<String> {
    let sym = match (attr, data as i32) {
        ("layout_width" | "layout_height", -1) => "match_parent",
        ("layout_width" | "layout_height", -2) => "wrap_content",
        ("orientation", 0) => "horizontal",
        ("orientation", 1) => "vertical",
        ("visibility", 0) => "visible",
        ("visibility", 1) => "invisible",
        ("visibility", 2) => "gone",
        ("gravity" | "layout_gravity", _) => return gravity_flags(data),
        _ => return None,
    };
    Some(sym.to_string())
}

/// `Gravity` packs one value per axis (bits 4-6 vertical, 0-2 horizontal,
/// bit 23 relative start/end); clip bits are ignored.
fn gravity_flags(data: u32) -> Option<String> {
    let v = match data & 0x70 {
        0 => None,
        0x10 => Some("center_vertical"),
        0x30 => Some("top"),
        0x50 => Some("bottom"),
        0x70 => Some("fill_vertical"),
        _ => return None,
    };
    let relative = data & 0x0080_0000 != 0;
    let h = match (data & 0x07, relative) {
        (0, _) => None,
        (0x01, _) => Some("center_horizontal"),
        (0x03, false) => Some("left"),
        (0x05, false) => Some("right"),
        (0x03, true) => Some("start"),
        (0x05, true) => Some("end"),
        (0x07, _) => Some("fill_horizontal"),
        _ => return None,
    };
    match (v, h) {
        (Some("center_vertical"), Some("center_horizontal")) => Some("center".into()),
        (Some("fill_vertical"), Some("fill_horizontal")) => Some("fill".into()),
        (Some(v), Some(h)) => Some(format!("{v}|{h}")),
        (Some(x), None) | (None, Some(x)) => Some(x.into()),
        (None, None) => None,
    }
}

/// Android binary XML parser.
pub struct AxmlParser;

impl AxmlParser {
    /// Parse binary XML data into an AxmlDocument.
    pub fn parse(data: &[u8]) -> Result<AxmlDocument, AxmlError> {
        if data.len() < 8 {
            return Err(AxmlError::TooShort);
        }

        let mut parser = AxmlParserState::new(data);
        parser.parse()?;

        Ok(AxmlDocument { root: parser.root })
    }
}

struct AxmlParserState<'a> {
    data: &'a [u8],
    pos: usize,
    strings: Vec<String>,
    resource_ids: Vec<u32>,
    root: Option<AxmlElement>,
    stack: Vec<AxmlElement>,
}

impl<'a> AxmlParserState<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            strings: Vec::new(),
            resource_ids: Vec::new(),
            root: None,
            stack: Vec::new(),
        }
    }

    fn parse(&mut self) -> Result<(), AxmlError> {
        // Read AXML header
        let chunk_type = self.read_u16()?;
        if chunk_type != CHUNK_AXML {
            return Err(AxmlError::InvalidHeader);
        }
        let _header_size = self.read_u16()?;
        let _file_size = self.read_u32()?;

        // Process chunks
        while self.pos < self.data.len() {
            if self.pos + 4 > self.data.len() {
                break;
            }

            let chunk_type = self.read_u16()?;
            let header_size = self.read_u16()?;
            let chunk_size = self.read_u32()? as usize;

            if chunk_size < 8 || self.pos - 8 + chunk_size > self.data.len() {
                break;
            }

            let chunk_start = self.pos - 8;
            let chunk_data_start = chunk_start + header_size as usize;
            let chunk_end = chunk_start + chunk_size;
            if header_size < 8 || chunk_data_start > chunk_end {
                break;
            }

            match chunk_type {
                CHUNK_STRING_POOL => {
                    self.parse_string_pool(chunk_start, chunk_end)?;
                }
                CHUNK_RESOURCE_MAP => {
                    self.pos = chunk_data_start;
                    self.parse_resource_map(chunk_data_start, chunk_end)?;
                }
                CHUNK_START_TAG => {
                    self.parse_start_tag(chunk_data_start, chunk_end)?;
                }
                CHUNK_END_TAG => {
                    self.parse_end_tag()?;
                }
                CHUNK_START_NAMESPACE | CHUNK_END_NAMESPACE => {
                    // Skip namespace chunks
                }
                _ => {
                    // Unknown chunk, skip
                }
            }

            self.pos = chunk_end;
        }

        Ok(())
    }

    /// `ResStringPool_header`: string data lives at `chunk_start +
    /// stringsStart`, not right after the offsets (a pool with styles has
    /// style offsets in between).
    fn parse_string_pool(&mut self, chunk_start: usize, chunk_end: usize) -> Result<(), AxmlError> {
        let string_count = self.read_u32()? as usize;
        let style_count = self.read_u32()? as usize;
        let flags = self.read_u32()?;
        let strings_start = self.read_u32()? as usize;
        let styles_start = self.read_u32()? as usize;

        let is_utf8 = (flags & (1 << 8)) != 0;

        // Every offset is 4 bytes inside the chunk: a bogus count must not
        // become a multi-GB allocation.
        let offsets_len = string_count
            .checked_add(style_count)
            .and_then(|n| n.checked_mul(4))
            .ok_or(AxmlError::Malformed("string pool counts overflow"))?;
        let offsets_start = chunk_start + 28;
        if offsets_start + offsets_len > chunk_end {
            return Err(AxmlError::Malformed("string pool offsets past chunk end"));
        }

        self.pos = offsets_start;
        let mut offsets = Vec::with_capacity(string_count);
        for _ in 0..string_count {
            offsets.push(self.read_u32()? as usize);
        }

        let data_base = chunk_start.saturating_add(strings_start);
        let data_end = if style_count > 0 && styles_start > strings_start {
            chunk_start.saturating_add(styles_start).min(chunk_end)
        } else {
            chunk_end
        };

        // Offsets may all point at one long string: each is decoded on its
        // own, so a pool of S bytes could decode to about S²/4 bytes.
        let mut decoded = 0usize;
        for &offset in &offsets {
            let abs_pos = data_base.saturating_add(offset);
            if abs_pos >= data_end {
                self.strings.push(String::new());
                continue;
            }

            let s = if is_utf8 {
                self.read_utf8_string(abs_pos, data_end)
            } else {
                self.read_utf16_string(abs_pos, data_end)
            };
            decoded = decoded.saturating_add(s.len());
            if decoded > MAX_POOL_DECODED_BYTES {
                return Err(AxmlError::Malformed("string pool decodes too large"));
            }
            self.strings.push(s);
        }

        self.pos = chunk_end;
        Ok(())
    }

    /// UTF-8 pool entry: a varint UTF-16 length (1 or 2 bytes, high bit of
    /// the first marks the 2-byte form), then a varint byte length, then the
    /// bytes.
    fn read_utf8_string(&self, pos: usize, limit: usize) -> String {
        let varint8 = |p: usize| -> Option<(usize, usize)> {
            let b0 = *self.data.get(p).filter(|_| p < limit)? as usize;
            if b0 & 0x80 != 0 {
                let b1 = *self.data.get(p + 1).filter(|_| p + 1 < limit)? as usize;
                Some((((b0 & 0x7F) << 8) | b1, p + 2))
            } else {
                Some((b0, p + 1))
            }
        };
        let Some((_chars, p)) = varint8(pos) else {
            return String::new();
        };
        let Some((byte_count, str_start)) = varint8(p) else {
            return String::new();
        };
        let str_end = str_start.saturating_add(byte_count).min(limit);
        if str_start >= str_end {
            return String::new();
        }
        String::from_utf8_lossy(&self.data[str_start..str_end]).to_string()
    }

    /// UTF-16 pool entry: a u16 length, or two when the first has bit 15
    /// set, then that many code units.
    fn read_utf16_string(&self, pos: usize, limit: usize) -> String {
        let unit = |p: usize| -> Option<u16> {
            if p + 2 > limit {
                return None;
            }
            Some(u16::from_le_bytes([self.data[p], self.data[p + 1]]))
        };
        let Some(first) = unit(pos) else {
            return String::new();
        };
        let (char_count, str_start) = if first & 0x8000 != 0 {
            let Some(second) = unit(pos + 2) else {
                return String::new();
            };
            (
                (((first & 0x7FFF) as usize) << 16) | second as usize,
                pos + 4,
            )
        } else {
            (first as usize, pos + 2)
        };

        let max_units = limit.saturating_sub(str_start) / 2;
        let mut chars = Vec::with_capacity(char_count.min(max_units));
        let mut p = str_start;
        for _ in 0..char_count {
            let Some(c) = unit(p) else { break };
            if c == 0 {
                break;
            }
            chars.push(c);
            p += 2;
        }

        String::from_utf16_lossy(&chars)
    }

    fn parse_resource_map(
        &mut self,
        _data_start: usize,
        chunk_end: usize,
    ) -> Result<(), AxmlError> {
        while self.pos + 4 <= chunk_end {
            let val = self.read_u32()?;
            self.resource_ids.push(val);
        }
        Ok(())
    }

    /// `ResXMLTree_attrExt` at `ext_start`; attribute `i` is at
    /// `ext_start + attributeStart + i * attributeSize`.
    fn parse_start_tag(&mut self, ext_start: usize, chunk_end: usize) -> Result<(), AxmlError> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(AxmlError::TooDeep(MAX_DEPTH));
        }
        self.pos = ext_start;
        let _ns_idx = self.read_i32()?;
        let name_idx = self.read_i32()?;
        let attr_start = self.read_u16()? as usize;
        let attr_size = self.read_u16()? as usize;
        let attr_count = self.read_u16()? as usize;
        let _id_index = self.read_u16()?;
        let _class_index = self.read_u16()?;
        let _style_index = self.read_u16()?;

        if attr_count > 0 && attr_size < 20 {
            return Err(AxmlError::Malformed("attribute size below 20 bytes"));
        }
        if ext_start + attr_start + attr_count * attr_size > chunk_end {
            return Err(AxmlError::Malformed("attributes past chunk end"));
        }

        let name = self.get_string(name_idx as usize);

        let mut attributes = HashMap::new();
        for i in 0..attr_count {
            self.pos = ext_start + attr_start + i * attr_size;
            let _attr_ns = self.read_i32()?;
            let attr_name_idx = self.read_u32()?;
            let attr_raw_value = self.read_i32()?;
            let _attr_type_size = self.read_u16()?;
            let _attr_type_res = self.read_u8()?;
            let attr_data_type = self.read_u8()?;
            let attr_data = self.read_u32()?;

            let attr_name = self.attr_name(attr_name_idx);

            let attr_value = match attr_data_type {
                // String reference
                0x03 => self.get_string(attr_data as usize),
                // Integers: a symbolic name for the framework enums/flags
                // the style mapper reads, else signed decimal / hex
                0x10 | 0x11 => match symbolic_value(&attr_name, attr_data) {
                    Some(sym) => sym,
                    None if attr_data_type == 0x10 => (attr_data as i32).to_string(),
                    None => format!("0x{:08x}", attr_data),
                },
                // Boolean
                0x12 => if attr_data != 0 { "true" } else { "false" }.to_string(),
                // Color (aapt stores all four forms as ARGB8)
                0x1C..=0x1F => format!("#{:08x}", attr_data),
                // Dimension
                0x05 => {
                    let value = complex_to_float(attr_data);
                    let unit = match attr_data & 0xF {
                        0 => "px",
                        1 => "dp",
                        2 => "sp",
                        3 => "pt",
                        4 => "in",
                        5 => "mm",
                        _ => "",
                    };
                    format!("{value}{unit}")
                }
                // Fraction (`%` of self, `%p` of parent)
                0x06 => {
                    let value = complex_to_float(attr_data) * 100.0;
                    if attr_data & 0xF == 1 {
                        format!("{value}%p")
                    } else {
                        format!("{value}%")
                    }
                }
                // Float
                0x04 => f32::from_bits(attr_data).to_string(),
                // Resource reference
                0x01 => {
                    if attr_raw_value >= 0 {
                        self.get_string(attr_raw_value as usize)
                    } else {
                        format!("@0x{:08x}", attr_data)
                    }
                }
                // Fallback: try string, then raw value
                _ => {
                    if attr_raw_value >= 0 {
                        self.get_string(attr_raw_value as usize)
                    } else {
                        format!("{}", attr_data)
                    }
                }
            };

            attributes.insert(attr_name, attr_value);
        }

        let element = AxmlElement {
            name,
            namespace: None,
            attributes,
            children: Vec::new(),
        };

        self.stack.push(element);
        Ok(())
    }

    /// An attribute's name. Release builds often strip framework attribute
    /// names from the pool; the resource map still says which they are.
    fn attr_name(&self, idx: u32) -> String {
        let name = self.get_string(idx as usize);
        if !name.is_empty() {
            return name;
        }
        self.resource_ids
            .get(idx as usize)
            .and_then(|id| framework_attr_name(*id))
            .map(str::to_string)
            .unwrap_or_default()
    }

    fn parse_end_tag(&mut self) -> Result<(), AxmlError> {
        let _line = self.read_u32()?;
        let _comment = self.read_u32()?;
        let _ns_idx = self.read_i32()?;
        let _name_idx = self.read_i32()?;

        if let Some(element) = self.stack.pop() {
            if let Some(parent) = self.stack.last_mut() {
                parent.children.push(element);
            } else {
                self.root = Some(element);
            }
        }

        Ok(())
    }

    fn get_string(&self, index: usize) -> String {
        self.strings.get(index).cloned().unwrap_or_default()
    }

    fn read_u8(&mut self) -> Result<u8, AxmlError> {
        if self.pos >= self.data.len() {
            return Err(AxmlError::UnexpectedEof);
        }
        let val = self.data[self.pos];
        self.pos += 1;
        Ok(val)
    }

    fn read_u16(&mut self) -> Result<u16, AxmlError> {
        if self.pos + 2 > self.data.len() {
            return Err(AxmlError::UnexpectedEof);
        }
        let val = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(val)
    }

    fn read_u32(&mut self) -> Result<u32, AxmlError> {
        if self.pos + 4 > self.data.len() {
            return Err(AxmlError::UnexpectedEof);
        }
        let val = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(val)
    }

    fn read_i32(&mut self) -> Result<i32, AxmlError> {
        Ok(self.read_u32()? as i32)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AxmlError {
    #[error("Data too short for AXML")]
    TooShort,
    #[error("Invalid AXML header")]
    InvalidHeader,
    #[error("Unexpected end of data")]
    UnexpectedEof,
    #[error("Malformed AXML: {0}")]
    Malformed(&'static str),
    #[error("AXML nesting deeper than {0} elements")]
    TooDeep(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_too_short() {
        let result = AxmlParser::parse(&[0, 0, 0]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_header() {
        let result = AxmlParser::parse(&[0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert!(result.is_err());
    }

    // --- a minimal aapt-style writer, so the fixtures are readable ---

    struct Pool {
        strings: Vec<String>,
        utf8: bool,
        styles: u32,
    }

    impl Pool {
        fn idx(&mut self, s: &str) -> u32 {
            match self.strings.iter().position(|x| x == s) {
                Some(i) => i as u32,
                None => {
                    self.strings.push(s.to_string());
                    (self.strings.len() - 1) as u32
                }
            }
        }

        fn chunk(&self) -> Vec<u8> {
            let mut data = Vec::new();
            let mut offsets = Vec::new();
            for s in &self.strings {
                offsets.push(data.len() as u32);
                if self.utf8 {
                    let n16 = s.encode_utf16().count();
                    for n in [n16, s.len()] {
                        if n > 0x7F {
                            data.extend_from_slice(&[0x80 | (n >> 8) as u8, n as u8]);
                        } else {
                            data.push(n as u8);
                        }
                    }
                    data.extend_from_slice(s.as_bytes());
                    data.push(0);
                } else {
                    let u: Vec<u16> = s.encode_utf16().collect();
                    if u.len() > 0x7FFF {
                        data.extend_from_slice(&(0x8000 | (u.len() >> 16) as u16).to_le_bytes());
                    }
                    data.extend_from_slice(&(u.len() as u16).to_le_bytes());
                    for c in u {
                        data.extend_from_slice(&c.to_le_bytes());
                    }
                    data.extend_from_slice(&[0, 0]);
                }
            }
            while data.len() % 4 != 0 {
                data.push(0);
            }
            // Style offsets sit between the string offsets and the data.
            let n = self.strings.len() as u32;
            let strings_start = 28 + 4 * (n + self.styles);
            let styles_data = if self.styles > 0 {
                4 * self.styles + 8
            } else {
                0
            };
            let styles_start = if self.styles > 0 {
                strings_start + data.len() as u32
            } else {
                0
            };
            let size = strings_start + data.len() as u32 + styles_data;
            let mut c = Vec::new();
            c.extend_from_slice(&0x0001u16.to_le_bytes());
            c.extend_from_slice(&28u16.to_le_bytes());
            c.extend_from_slice(&size.to_le_bytes());
            c.extend_from_slice(&n.to_le_bytes());
            c.extend_from_slice(&self.styles.to_le_bytes());
            c.extend_from_slice(&(if self.utf8 { 1u32 << 8 } else { 0 }).to_le_bytes());
            c.extend_from_slice(&strings_start.to_le_bytes());
            c.extend_from_slice(&styles_start.to_le_bytes());
            for o in offsets {
                c.extend_from_slice(&o.to_le_bytes());
            }
            for i in 0..self.styles {
                c.extend_from_slice(&(i * 4).to_le_bytes());
            }
            c.extend_from_slice(&data);
            for _ in 0..self.styles {
                c.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            }
            if self.styles > 0 {
                c.extend_from_slice(&[0xFF; 8]);
            }
            c
        }
    }

    /// (attribute name, type, data); `attr_size` > 20 pads each attribute.
    type Attr<'a> = (&'a str, u8, u32);

    fn doc_with(
        mut pool: Pool,
        res_ids: &[u32],
        tags: &[(&str, Vec<Attr>)],
        attr_size: u16,
    ) -> Vec<u8> {
        // Resource-mapped names must come first in the pool.
        let mut body_tags = Vec::new();
        for (name, attrs) in tags {
            let n = pool.idx(name);
            let attrs: Vec<(u32, u8, u32)> = attrs
                .iter()
                .map(|(a, t, d)| (pool.idx(a), *t, *d))
                .collect();
            body_tags.push((n, attrs));
        }
        let mut body = pool.chunk();
        if !res_ids.is_empty() {
            body.extend_from_slice(&0x0180u16.to_le_bytes());
            body.extend_from_slice(&8u16.to_le_bytes());
            body.extend_from_slice(&(8 + 4 * res_ids.len() as u32).to_le_bytes());
            for id in res_ids {
                body.extend_from_slice(&id.to_le_bytes());
            }
        }
        for (name, attrs) in &body_tags {
            body.extend_from_slice(&0x0102u16.to_le_bytes());
            body.extend_from_slice(&16u16.to_le_bytes());
            let size = 16 + 20 + attr_size as u32 * attrs.len() as u32;
            body.extend_from_slice(&size.to_le_bytes());
            body.extend_from_slice(&1u32.to_le_bytes());
            body.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            body.extend_from_slice(&(-1i32).to_le_bytes());
            body.extend_from_slice(&name.to_le_bytes());
            body.extend_from_slice(&20u16.to_le_bytes());
            body.extend_from_slice(&attr_size.to_le_bytes());
            body.extend_from_slice(&(attrs.len() as u16).to_le_bytes());
            body.extend_from_slice(&[0u8; 6]);
            for (a, t, d) in attrs {
                body.extend_from_slice(&(-1i32).to_le_bytes());
                body.extend_from_slice(&a.to_le_bytes());
                let raw: i32 = if *t == 0x03 { *d as i32 } else { -1 };
                body.extend_from_slice(&raw.to_le_bytes());
                body.extend_from_slice(&8u16.to_le_bytes());
                body.push(0);
                body.push(*t);
                body.extend_from_slice(&d.to_le_bytes());
                body.extend(std::iter::repeat_n(0xEE, attr_size as usize - 20));
            }
        }
        for (name, _) in body_tags.iter().rev() {
            body.extend_from_slice(&0x0103u16.to_le_bytes());
            body.extend_from_slice(&16u16.to_le_bytes());
            body.extend_from_slice(&24u32.to_le_bytes());
            body.extend_from_slice(&1u32.to_le_bytes());
            body.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            body.extend_from_slice(&(-1i32).to_le_bytes());
            body.extend_from_slice(&name.to_le_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(&0x0003u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn pool(utf8: bool) -> Pool {
        Pool {
            strings: vec![],
            utf8,
            styles: 0,
        }
    }

    fn root_attrs(data: &[u8]) -> HashMap<String, String> {
        AxmlParser::parse(data).unwrap().root.unwrap().attributes
    }

    /// `TypedValue` complex: mantissa << 8 | radix << 4 | unit.
    fn complex(mantissa: i32, radix: u32, unit: u32) -> u32 {
        ((mantissa as u32) << 8) | (radix << 4) | unit
    }

    #[test]
    fn int_dec_is_signed_and_layout_sizes_are_symbolic() {
        // VEL-019: match_parent/wrap_content used to be 4294967295/4294967294.
        let d = doc_with(
            pool(false),
            &[],
            &[(
                "View",
                vec![
                    ("layout_width", 0x10, 0xFFFF_FFFF),
                    ("layout_height", 0x10, 0xFFFF_FFFE),
                    ("maxLines", 0x10, (-3i32) as u32),
                ],
            )],
            20,
        );
        let a = root_attrs(&d);
        assert_eq!(a["layout_width"], "match_parent");
        assert_eq!(a["layout_height"], "wrap_content");
        assert_eq!(a["maxLines"], "-3");
    }

    #[test]
    fn complex_dimensions_keep_sign_fraction_and_unit() {
        // VEL-100: radix bits and sign were ignored; pt/in/mm lost the unit.
        let d = doc_with(
            pool(false),
            &[],
            &[(
                "View",
                vec![
                    ("a", 0x05, complex(-8, 0, 1)),
                    // radix 1 is 16p7: 14.5 * 2^7 = 1856
                    ("b", 0x05, complex(1856, 1, 2)),
                    ("c", 0x05, complex(12, 0, 3)),
                    ("d", 0x05, complex(1, 0, 4)),
                    ("e", 0x05, complex(5, 0, 5)),
                    ("f", 0x06, complex(1 << 22, 3, 1)),
                ],
            )],
            20,
        );
        let a = root_attrs(&d);
        assert_eq!(a["a"], "-8dp");
        assert_eq!(a["b"], "14.5sp");
        assert_eq!(a["c"], "12pt");
        assert_eq!(a["d"], "1in");
        assert_eq!(a["e"], "5mm");
        assert_eq!(a["f"], "50%p");
    }

    #[test]
    fn long_strings_use_the_varint_length_forms() {
        // VEL-101: only the 1-byte UTF-8 length was read; UTF-16 lengths
        // over 0x7FFF use two units.
        let long8 = "x".repeat(200);
        let mut p = pool(true);
        let v = p.idx(&long8);
        let d = doc_with(p, &[], &[("View", vec![("text", 0x03, v)])], 20);
        assert_eq!(root_attrs(&d)["text"], long8);

        let long16 = "y".repeat(40_000);
        let mut p = pool(false);
        let v = p.idx(&long16);
        let d = doc_with(p, &[], &[("View", vec![("text", 0x03, v)])], 20);
        assert_eq!(root_attrs(&d)["text"], long16);
    }

    #[test]
    fn string_data_is_read_from_strings_start_when_styles_exist() {
        let mut p = pool(false);
        p.styles = 2;
        let v = p.idx("hello");
        let d = doc_with(p, &[], &[("TextView", vec![("text", 0x03, v)])], 20);
        let root = AxmlParser::parse(&d).unwrap().root.unwrap();
        assert_eq!(root.name, "TextView");
        assert_eq!(root.attributes["text"], "hello");
    }

    #[test]
    fn attribute_size_from_the_header_is_honoured() {
        let d = doc_with(
            pool(false),
            &[],
            &[(
                "View",
                vec![("layout_width", 0x10, 0xFFFF_FFFF), ("enabled", 0x12, 1)],
            )],
            24,
        );
        let a = root_attrs(&d);
        assert_eq!(a["layout_width"], "match_parent");
        assert_eq!(a["enabled"], "true");
    }

    #[test]
    fn stripped_attribute_names_resolve_by_resource_id() {
        // Pool entries 0..2 are the mapped (and here emptied) names.
        let mut p = pool(false);
        p.idx("");
        p.strings.push(String::new());
        let d = {
            let mut p2 = p;
            // indices 0 and 1 are "" and "" -> layout_width / gravity
            p2.idx("FrameLayout");
            let mut body = doc_with(p2, &[0x010100f4, 0x010100af], &[], 20);
            // Splice a start/end tag referencing name idx 2 and attrs 0, 1.
            let mut t = Vec::new();
            t.extend_from_slice(&0x0102u16.to_le_bytes());
            t.extend_from_slice(&16u16.to_le_bytes());
            t.extend_from_slice(&(16u32 + 20 + 40).to_le_bytes());
            t.extend_from_slice(&1u32.to_le_bytes());
            t.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            t.extend_from_slice(&(-1i32).to_le_bytes());
            t.extend_from_slice(&2u32.to_le_bytes());
            t.extend_from_slice(&20u16.to_le_bytes());
            t.extend_from_slice(&20u16.to_le_bytes());
            t.extend_from_slice(&2u16.to_le_bytes());
            t.extend_from_slice(&[0u8; 6]);
            for (name, ty, data) in [(0u32, 0x10u8, 0xFFFF_FFFFu32), (1, 0x11, 0x11)] {
                t.extend_from_slice(&(-1i32).to_le_bytes());
                t.extend_from_slice(&name.to_le_bytes());
                t.extend_from_slice(&(-1i32).to_le_bytes());
                t.extend_from_slice(&8u16.to_le_bytes());
                t.push(0);
                t.push(ty);
                t.extend_from_slice(&data.to_le_bytes());
            }
            t.extend_from_slice(&0x0103u16.to_le_bytes());
            t.extend_from_slice(&16u16.to_le_bytes());
            t.extend_from_slice(&24u32.to_le_bytes());
            t.extend_from_slice(&[0u8; 16]);
            body.extend_from_slice(&t);
            let len = body.len() as u32;
            body[4..8].copy_from_slice(&len.to_le_bytes());
            body
        };
        let root = AxmlParser::parse(&d).unwrap().root.unwrap();
        assert_eq!(root.name, "FrameLayout");
        assert_eq!(root.attributes["layout_width"], "match_parent");
        assert_eq!(root.attributes["gravity"], "center");
    }

    #[test]
    fn integer_enums_and_flags_become_their_xml_names() {
        // VEL-S24: the style mapper reads the names aapt compiled away.
        let d = doc_with(
            pool(false),
            &[],
            &[(
                "LinearLayout",
                vec![
                    ("orientation", 0x10, 1),
                    ("visibility", 0x10, 1),
                    ("gravity", 0x11, 0x50 | 0x800005),
                    ("layout_gravity", 0x11, 0x01),
                ],
            )],
            20,
        );
        let a = root_attrs(&d);
        assert_eq!(a["orientation"], "vertical");
        assert_eq!(a["visibility"], "invisible");
        assert_eq!(a["gravity"], "bottom|end");
        assert_eq!(a["layout_gravity"], "center_horizontal");
    }

    /// Review MINOR-5: thousands of offsets at one long string decoded
    /// it once each (O(S²) memory from S bytes of input).
    #[test]
    fn a_pool_that_decodes_too_large_is_an_error() {
        let long = "x".repeat(0x7FFF);
        let mut data = vec![0xFF, 0xFF]; // UTF-16 length: unused
        data.extend_from_slice(&[0xFF, 0xFF]); // UTF-8 length 0x7FFF
        data.extend_from_slice(long.as_bytes());
        data.push(0);
        while data.len() % 4 != 0 {
            data.push(0);
        }
        let n: u32 = 4096; // 4096 x 32 KiB = 128 MiB decoded
        let strings_start = 28 + 4 * n;
        let size = strings_start + data.len() as u32;
        let mut c = Vec::new();
        c.extend_from_slice(&0x0001u16.to_le_bytes());
        c.extend_from_slice(&28u16.to_le_bytes());
        c.extend_from_slice(&size.to_le_bytes());
        c.extend_from_slice(&n.to_le_bytes());
        c.extend_from_slice(&0u32.to_le_bytes());
        c.extend_from_slice(&(1u32 << 8).to_le_bytes());
        c.extend_from_slice(&strings_start.to_le_bytes());
        c.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..n {
            c.extend_from_slice(&0u32.to_le_bytes());
        }
        c.extend_from_slice(&data);
        let mut d = Vec::new();
        d.extend_from_slice(&0x0003u16.to_le_bytes());
        d.extend_from_slice(&8u16.to_le_bytes());
        d.extend_from_slice(&(8 + c.len() as u32).to_le_bytes());
        d.extend_from_slice(&c);
        assert!(d.len() < 200_000);
        assert!(matches!(
            AxmlParser::parse(&d),
            Err(AxmlError::Malformed("string pool decodes too large"))
        ));
    }

    #[test]
    fn huge_counts_and_deep_nesting_are_errors_not_crashes() {
        // A string count of u32::MAX must not try to allocate 16 GB.
        let mut d = doc_with(pool(false), &[], &[("View", vec![])], 20);
        d[8 + 8..8 + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(AxmlParser::parse(&d).is_err());

        let tags: Vec<(&str, Vec<Attr>)> = (0..600).map(|_| ("V", vec![])).collect();
        let d = doc_with(pool(false), &[], &tags, 20);
        assert!(matches!(
            AxmlParser::parse(&d),
            Err(AxmlError::TooDeep(MAX_DEPTH))
        ));
    }
}
