//! Match the mod's WasmPreflight/ChicoryEngine admission profile before JIT compilation.
//! These limits apply to boot firmware, child modules, and passive kernels alike.

pub(crate) const MAX_MODULE_BYTES: usize = 1024 * 1024;

pub(crate) fn validate_module(wasm: &[u8]) -> Result<(), String> {
    if !(8..=MAX_MODULE_BYTES).contains(&wasm.len()) || wasm[..8] != *b"\0asm\x01\0\0\0" {
        return Err("WASM module size/header outside configured limits".into());
    }
    let mut module = Reader(&wasm[8..]);
    let (mut sections, mut total_locals, mut total_code) = (0, 0, 0);
    while !module.0.is_empty() {
        sections += 1;
        ensure(sections <= 64, "Too many sections")?;
        let id = module.byte()?;
        let size = module.u32()?;
        let mut section = module.slice(size)?;
        match id {
            0 => continue,
            1 => {
                for _ in 0..section.count(4096)? {
                    ensure(section.byte()? == 0x60, "Only function types are supported")?;
                    for _ in 0..section.count(128)? {
                        section.numeric_type()?;
                    }
                    for _ in 0..section.count(16)? {
                        section.numeric_type()?;
                    }
                }
            }
            2 => {
                for _ in 0..section.count(64)? {
                    section.name()?;
                    section.name()?;
                    ensure(section.byte()? == 0, "Only function imports are supported")?;
                    section.u32()?;
                }
            }
            3 => {
                for _ in 0..section.count(4096)? {
                    section.u32()?;
                }
            }
            4 => {
                for _ in 0..section.count(1)? {
                    ensure(
                        section.byte()? == 0x70 && section.u32()? == 1,
                        "Tables require funcref and an explicit maximum",
                    )?;
                    section.count(4096)?;
                    section.count(4096)?;
                }
            }
            5 => {
                for _ in 0..section.count(1)? {
                    let flags = section.count(1)?;
                    section.count(65536)?;
                    if flags == 1 {
                        section.count(65536)?;
                    }
                }
            }
            6 => {
                for _ in 0..section.count(4096)? {
                    section.numeric_type()?;
                    ensure(section.byte()? <= 1, "Invalid global mutability")?;
                    section.constant_expression()?;
                }
            }
            7 => {
                for _ in 0..section.count(4096)? {
                    section.name()?;
                    section.byte()?;
                    section.u32()?;
                }
            }
            8 => {
                section.u32()?;
            }
            9 => {
                for _ in 0..section.count(64)? {
                    ensure(
                        section.u32()? == 0,
                        "Only active MVP element segments are supported",
                    )?;
                    section.constant_expression()?;
                    for _ in 0..section.count(4096)? {
                        section.u32()?;
                    }
                }
            }
            10 => {
                for _ in 0..section.count(4096)? {
                    let size = section.u32()?;
                    total_code += size;
                    ensure(total_code <= 64 * 1024, "Decoded code limit exceeded")?;
                    let mut body = section.slice(size)?;
                    let mut locals = 0;
                    for _ in 0..body.count(1024)? {
                        locals += body.count(1024)?;
                        body.numeric_type()?;
                    }
                    total_locals += locals;
                    ensure(
                        locals <= 1024 && total_locals <= 65536,
                        "Local variable limit exceeded",
                    )?;
                }
            }
            11 => {
                for _ in 0..section.count(4096)? {
                    let flags = section.count(2)?;
                    if flags == 2 {
                        section.u32()?;
                    }
                    if flags != 1 {
                        section.constant_expression()?;
                    }
                    let size = section.u32()?;
                    section.slice(size)?;
                }
            }
            12 => {
                section.count(4096)?;
            }
            _ => return Err("Unsupported WASM section".into()),
        }
        ensure(section.0.is_empty(), "Invalid section size")?;
    }
    // Wasmtime performs full validation. Inspect decoded operators here to apply
    // the mod's additional instruction restrictions without rejecting bulk memory.
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CodeSectionEntry(body) = payload.map_err(|e| e.to_string())? {
            let mut ops = body.get_operators_reader().map_err(|e| e.to_string())?;
            while !ops.eof() {
                let op = ops.read().map_err(|e| e.to_string())?;
                let description = format!("{op:?}");
                let name = description
                    .split([' ', '{'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase();
                let forbidden = [
                    "V128", "I8X16", "I16X8", "I32X4", "I64X2", "F32X4", "F64X2", "ARRAY",
                    "STRUCT", "I31", "REFI31", "ANY", "EXTERN", "REFCAST", "REFTEST", "BRON",
                    "THROW", "TRY",
                ];
                ensure(
                    !name.contains("ATOMIC")
                        && !forbidden.iter().any(|prefix| name.starts_with(prefix)),
                    "Unsupported WASM instruction",
                )?;
            }
        }
    }
    Ok(())
}

fn ensure(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        let (&first, rest) = self.0.split_first().ok_or("Truncated WASM section")?;
        self.0 = rest;
        Ok(first)
    }
    fn u32(&mut self) -> Result<usize, String> {
        let mut result = 0_u64;
        for shift in (0..35).step_by(7) {
            let byte = self.byte()?;
            result |= u64::from(byte & 127) << shift;
            if byte & 128 == 0 {
                ensure(
                    result <= i32::MAX as u64,
                    "Declaration exceeds supported limits",
                )?;
                return Ok(result as usize);
            }
        }
        Err("Invalid unsigned LEB128".into())
    }
    fn count(&mut self, maximum: usize) -> Result<usize, String> {
        let value = self.u32()?;
        ensure(value <= maximum, "Declaration limit exceeded")?;
        Ok(value)
    }
    fn slice(&mut self, size: usize) -> Result<Self, String> {
        ensure(size <= self.0.len(), "Truncated WASM section")?;
        let (section, rest) = self.0.split_at(size);
        self.0 = rest;
        Ok(Self(section))
    }
    fn name(&mut self) -> Result<(), String> {
        let size = self.count(128)?;
        self.slice(size)?;
        Ok(())
    }
    fn numeric_type(&mut self) -> Result<(), String> {
        ensure(
            (0x7c..=0x7f).contains(&self.byte()?),
            "Only scalar numeric types are supported",
        )
    }
    fn constant_expression(&mut self) -> Result<(), String> {
        for _ in 0..128 {
            match self.byte()? {
                0x0b => return Ok(()),
                0x41 | 0x42 => {
                    let mut count = 0;
                    while self.byte()? & 128 != 0 {
                        count += 1;
                        ensure(count < 10, "Invalid constant")?;
                    }
                }
                0x43 => {
                    self.slice(4)?;
                }
                0x44 => {
                    self.slice(8)?;
                }
                0x23 => {
                    self.u32()?;
                }
                0x6a..=0x6c | 0x7c..=0x7e => {}
                _ => return Err("Unsupported constant expression".into()),
            }
        }
        Err("Constant expression too large".into())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_module;

    #[test]
    fn accepts_scalar_bulk_memory() {
        let wasm = wat::parse_str(
            "(module (memory 1) (func i32.const 0 i32.const 0 i32.const 4 memory.copy))",
        )
        .unwrap();
        validate_module(&wasm).unwrap();
    }

    #[test]
    fn rejects_simd_unbounded_tables_and_oversized_code() {
        for wat in [
            "(module (func v128.const i32x4 0 0 0 0 drop))",
            "(module (table 1 funcref))",
        ] {
            assert!(validate_module(&wat::parse_str(wat).unwrap()).is_err());
        }
        let wat = format!("(module (func {}))", "nop ".repeat(65536));
        assert!(validate_module(&wat::parse_str(wat).unwrap()).is_err());
    }

    #[test]
    fn rejects_truncated_and_oversized_modules() {
        assert!(validate_module(b"\0asm").is_err());
        assert!(validate_module(b"\0asm\x01\0\0\0\x01\x05").is_err());
        assert!(validate_module(&vec![0; 1024 * 1024 + 1]).is_err());
    }
}
