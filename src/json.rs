//! Parser y serializador JSON puro en Rust (`std` sin dependencias externas).
//!
//! Implementa RFC 8259 completo para comunicar el hipervisor Two Five Five (255)
//! con la API HTTP de Ollama (`/api/tags`, `/api/chat`, `tool_calls`) y el motor
//! de herramientas del agente IA.

use std::fmt;

/// Valor JSON genérico RFC 8259.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// Construye un `JsonValue::Object` de manera ergonómica.
    pub fn obj(entries: Vec<(&str, JsonValue)>) -> Self {
        JsonValue::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    /// Parsea una cadena JSON completa según RFC 8259.
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut parser = Parser::new(input);
        parser.skip_ws();
        let val = parser.parse_value(0)?;
        parser.skip_ws();
        if parser.pos < parser.bytes.len() {
            return Err(format!(
                "Caracteres inesperados tras el valor JSON en offset {}",
                parser.pos
            ));
        }
        Ok(val)
    }

    /// Serializa el valor a JSON compacto válido.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(true) => out.push_str("true"),
            JsonValue::Bool(false) => out.push_str("false"),
            JsonValue::Number(n) => {
                if n.is_finite() {
                    if n.fract() == 0.0 && *n >= (i64::MIN as f64) && *n <= (i64::MAX as f64) {
                        out.push_str(&format!("{}", *n as i64));
                    } else {
                        out.push_str(&format!("{}", n));
                    }
                } else {
                    out.push_str("null");
                }
            }
            JsonValue::Str(s) => write_escaped_str(s, out),
            JsonValue::Array(arr) => {
                out.push('[');
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            JsonValue::Object(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped_str(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }

    /// Obtiene un campo si `self` es un `JsonValue::Object`.
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Number(n) if n.is_finite() => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            JsonValue::Number(n) if n.is_finite() && *n >= 0.0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(o) => Some(o.as_slice()),
            _ => None,
        }
    }
}

impl fmt::Display for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json_string())
    }
}

fn write_escaped_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

const MAX_DEPTH: usize = 64;

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, String> {
        if depth > MAX_DEPTH {
            return Err("Profundidad máxima de anidamiento JSON excedida".to_string());
        }
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err("Fin inesperado de entrada JSON".to_string());
        };
        match b {
            b'n' => {
                if self.bytes[self.pos..].starts_with(b"null") {
                    self.pos += 4;
                    Ok(JsonValue::Null)
                } else {
                    Err(format!("Literal inválido en offset {}", self.pos))
                }
            }
            b't' => {
                if self.bytes[self.pos..].starts_with(b"true") {
                    self.pos += 4;
                    Ok(JsonValue::Bool(true))
                } else {
                    Err(format!("Literal inválido en offset {}", self.pos))
                }
            }
            b'f' => {
                if self.bytes[self.pos..].starts_with(b"false") {
                    self.pos += 5;
                    Ok(JsonValue::Bool(false))
                } else {
                    Err(format!("Literal inválido en offset {}", self.pos))
                }
            }
            b'"' => self.parse_string().map(JsonValue::Str),
            b'[' => self.parse_array(depth),
            b'{' => self.parse_object(depth),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(format!("Token JSON inesperado '{}' en offset {}", b as char, self.pos)),
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err("Se esperaba '\"'".to_string());
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let Some(esc) = self.peek() else {
                    return Err("Escape JSON incompleto al final".to_string());
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\x08'),
                    b'f' => out.push('\x0c'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            // Par sustituto UTF-16
                            if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                                return Err("Par sustituto UTF-16 incompleto en JSON".to_string());
                            }
                            self.pos += 2;
                            let low = self.parse_hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err("Par sustituto bajo UTF-16 inválido".to_string());
                            }
                            let cp = 0x10000
                                + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                            let ch = char::from_u32(cp)
                                .ok_or_else(|| "Punto de código Unicode inválido".to_string())?;
                            out.push(ch);
                        } else if (0xDC00..=0xDFFF).contains(&code) {
                            return Err("Par sustituto bajo UTF-16 huérfano".to_string());
                        } else {
                            let ch = char::from_u32(code as u32)
                                .ok_or_else(|| "Código \\uXXXX inválido".to_string())?;
                            out.push(ch);
                        }
                    }
                    other => {
                        return Err(format!("Secuencia de escape inválida \\{}", other as char));
                    }
                }
            } else if b < 0x20 {
                return Err(format!("Carácter de control sin escapar en string JSON: 0x{:02x}", b));
            } else {
                // Decodificar un carácter UTF-8 completo
                let ch = self.input[self.pos..]
                    .chars()
                    .next()
                    .ok_or_else(|| "UTF-8 inválido".to_string())?;
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        Err("String JSON sin comilla de cierre".to_string())
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("Escape \\uXXXX truncado".to_string());
        }
        let hex_str = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "Hex inválido".to_string())?;
        let val = u16::from_str_radix(hex_str, 16)
            .map_err(|_| format!("Hex inválido en \\u{}", hex_str))?;
        self.pos += 4;
        Ok(val)
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err("Número JSON con cero inicial inválido".to_string());
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(format!("Número JSON inválido en offset {}", start)),
        }

        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err("Se esperaba dígito tras '.' en número JSON".to_string());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err("Se esperaba dígito en exponente JSON".to_string());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        let slice = &self.input[start..self.pos];
        let num = slice
            .parse::<f64>()
            .map_err(|e| format!("Error parseando número '{}': {}", slice, e))?;
        Ok(JsonValue::Number(num))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, String> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(items));
                }
                _ => return Err(format!("Se esperaba ',' o ']' en arreglo en offset {}", self.pos)),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, String> {
        self.pos += 1; // '{'
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("Se esperaba ':' tras clave en offset {}", self.pos));
            }
            self.pos += 1;
            let val = self.parse_value(depth + 1)?;
            entries.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(entries));
                }
                _ => return Err(format!("Se esperaba ',' o '}}' en objeto en offset {}", self.pos)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_roundtrip_and_escapes() {
        let obj = JsonValue::obj(vec![
            ("name", JsonValue::Str("Debian \"Lab\"\nLinea2".to_string())),
            ("cpus", JsonValue::Number(4.0)),
            ("ok", JsonValue::Bool(true)),
            ("nothing", JsonValue::Null),
            (
                "list",
                JsonValue::Array(vec![JsonValue::Number(-12.5), JsonValue::Str("España".to_string())]),
            ),
        ]);
        let s = obj.to_json_string();
        let parsed = JsonValue::parse(&s).expect("roundtrip parse");
        assert_eq!(parsed.get("name").and_then(|v| v.as_str()), Some("Debian \"Lab\"\nLinea2"));
        assert_eq!(parsed.get("cpus").and_then(|v| v.as_i64()), Some(4));
        assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(parsed.get("nothing"), Some(&JsonValue::Null));
        let arr = parsed.get("list").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr[0], JsonValue::Number(-12.5));
        assert_eq!(arr[1].as_str(), Some("España"));
    }

    #[test]
    fn test_json_unicode_and_surrogate_pairs() {
        let raw = r#"{"msg":"Hola \u00e1rbol \uD83D\uDE80"}"#;
        let v = JsonValue::parse(raw).expect("unicode parse");
        assert_eq!(v.get("msg").and_then(|x| x.as_str()), Some("Hola árbol 🚀"));
    }

    #[test]
    fn test_json_parse_ollama_chat_tool_calls() {
        let raw = r#"{
            "model": "northstar-4b:latest",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "function": {
                            "index": 0,
                            "name": "create_vm",
                            "arguments": {
                                "name": "Kali Lab",
                                "cpus": 2,
                                "ram_mb": 2048
                            }
                        }
                    }
                ]
            },
            "done": true
        }"#;
        let v = JsonValue::parse(raw).expect("parse ollama tool_calls");
        let tc = &v.get("message").unwrap().get("tool_calls").unwrap().as_array().unwrap()[0];
        let func = tc.get("function").unwrap();
        assert_eq!(func.get("name").and_then(|x| x.as_str()), Some("create_vm"));
        assert_eq!(func.get("arguments").unwrap().get("cpus").and_then(|x| x.as_u64()), Some(2));
    }

    #[test]
    fn test_json_invalid_inputs() {
        assert!(JsonValue::parse("").is_err());
        assert!(JsonValue::parse("{bad}").is_err());
        assert!(JsonValue::parse("[1, 2,]").is_err());
        assert!(JsonValue::parse("01").is_err());
        assert!(JsonValue::parse("{\"a\":1} extra").is_err());
    }
}
