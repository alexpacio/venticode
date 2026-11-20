//! TypeScript type stripping with swc, as pi_agent_rust's `transpile_typescript_module`.
use std::path::Path;
use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
use swc_ecma_ast::{Module, Pass, Program};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::resolver;
use swc_ecma_transforms_typescript::strip;

pub fn transpile(source: &str, name: &str) -> Result<String, String> {
    GLOBALS.set(&Globals::new(), || {
        let cm: Lrc<SourceMap> = Lrc::default();
        let fm = cm.new_source_file(FileName::Custom(name.to_owned()).into(), source.to_owned());
        let syntax = Syntax::Typescript(TsSyntax {
            tsx: Path::new(name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tsx")),
            decorators: true,
            ..Default::default()
        });
        let module: Module = Parser::new(syntax, StringInput::from(&*fm), None)
            .parse_module()
            .map_err(|e| format!("parse {name}: {:?}", e.kind()))?;
        let (unresolved, top_level) = (Mark::new(), Mark::new());
        let mut program = Program::Module(module);
        resolver(unresolved, top_level, true).process(&mut program);
        strip(unresolved, top_level).process(&mut program);
        let Program::Module(module) = program else {
            return Err(format!("transpile {name}: expected module"));
        };
        let mut buf = Vec::new();
        Emitter {
            cfg: swc_ecma_codegen::Config::default(),
            comments: None,
            cm: cm.clone(),
            wr: JsWriter::new(cm, "\n", &mut buf, None),
        }
        .emit_module(&module)
        .map_err(|e| format!("emit {name}: {e}"))?;
        String::from_utf8(buf).map_err(|e| format!("utf8 {name}: {e}"))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_types_and_type_only_imports() {
        let out = super::transpile(
            "import type { X } from 'a';\nimport { Type, type Static } from 'typebox';\nconst s = Type.String();\nexport default function f(p: X): number { return 1 as number; }",
            "x.ts",
        )
        .unwrap();
        assert!(
            !out.contains("from 'a'") && !out.contains("from \"a\""),
            "{out}"
        );
        assert!(out.contains("import { Type } from 'typebox'"), "{out}");
        assert!(!out.contains(": number"), "{out}");
    }
}
