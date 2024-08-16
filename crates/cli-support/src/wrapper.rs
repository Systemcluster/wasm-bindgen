use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{bail, Error};

use crate::js::Context;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    CommonJS,
    ESModule,
}
impl ModuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleKind::CommonJS => "cjs",
            ModuleKind::ESModule => "mjs",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "cjs" => Some(ModuleKind::CommonJS),
            "mjs" => Some(ModuleKind::ESModule),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    Bundler,
    Node,
    Web,
    NoModules,
    Deno,
}
impl ImportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ImportKind::Bundler => "bundler",
            ImportKind::Node => "node",
            ImportKind::Web => "web",
            ImportKind::NoModules => "no-modules",
            ImportKind::Deno => "deno",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "bundler" => Some(ImportKind::Bundler),
            "node" => Some(ImportKind::Node),
            "web" => Some(ImportKind::Web),
            "no-modules" => Some(ImportKind::NoModules),
            "deno" => Some(ImportKind::Deno),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionKind {
    Common,
    ModuleSpecific,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperOutput {
    pub wrapper: Wrapper,
    pub js: String,
    pub ts: Option<String>,
    pub start: Option<String>,
    pub snippets: HashMap<String, Vec<String>>,
    pub local_modules: HashMap<String, String>,
    pub npm_dependencies: HashMap<String, (PathBuf, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapper {
    pub name: Option<String>,
    pub module_kind: ModuleKind,
    pub import_kind: ImportKind,
    pub extension_kind: ExtensionKind,
}
impl Default for Wrapper {
    fn default() -> Self {
        Wrapper {
            name: None,
            module_kind: ModuleKind::CommonJS,
            import_kind: ImportKind::Bundler,
            extension_kind: ExtensionKind::Common,
        }
    }
}
impl Wrapper {
    pub fn new(
        module_kind: ModuleKind,
        import_kind: ImportKind,
        extension_kind: ExtensionKind,
        name: Option<String>,
    ) -> Self {
        Wrapper {
            name,
            module_kind,
            import_kind,
            extension_kind,
        }
    }

    fn js_import_header(&self, cx: &Context) -> Result<String, Error> {
        let mut imports = String::new();
        if cx.config.omit_imports {
            return Ok(imports);
        }
        match &self.import_kind {
            ImportKind::NoModules { .. } => {
                if let Some((module, _items)) = cx.js_imports.iter().next() {
                    bail!(
                        "importing from `{}` isn't supported with `--target no-modules`",
                        module
                    );
                }
            }
            ImportKind::Node if self.module_kind == ModuleKind::CommonJS => {
                for (module, items) in crate::sorted_iter(&cx.js_imports) {
                    imports.push_str("const { ");
                    for (i, (item, rename)) in items.iter().enumerate() {
                        if i > 0 {
                            imports.push_str(", ");
                        }
                        imports.push_str(item);
                        if let Some(other) = rename {
                            imports.push_str(": ");
                            imports.push_str(other)
                        }
                    }
                    if module.starts_with('.') || PathBuf::from(module).is_absolute() {
                        imports.push_str(" } = require(String.raw`");
                    } else {
                        imports.push_str(" } = require(`");
                    }
                    imports.push_str(module);
                    imports.push_str("`);\n");
                }
            }
            _ => {
                for (module, items) in crate::sorted_iter(&cx.js_imports) {
                    imports.push_str("import { ");
                    for (i, (item, rename)) in items.iter().enumerate() {
                        if i > 0 {
                            imports.push_str(", ");
                        }
                        imports.push_str(item);
                        if let Some(other) = rename {
                            imports.push_str(" as ");
                            imports.push_str(other)
                        }
                    }
                    imports.push_str(" } from '");
                    imports.push_str(module);
                    imports.push_str("';\n");
                }
            }
        }
        Ok(imports)
    }

    pub fn wrap(
        &self,
        cx: &Context,
        name: &str,
        needs_manual_start: bool,
    ) -> Result<WrapperOutput, Error> {
        let mut ts;
        let mut js = String::new();
        let mut start = None;

        if let ImportKind::NoModules = &self.import_kind {
            js.push_str(&format!(
                "let {};\n(function() {{\n",
                self.name.as_deref().unwrap_or(name)
            ));
        }

        // Depending on the output mode, generate necessary glue to actually
        // import the wasm file in one way or another.
        let mut init = (String::new(), String::new());
        let mut footer = String::new();
        let mut imports = self.js_import_header(cx)?;
        match &self.import_kind {
            // In `--target no-modules` mode we need to both expose a name on
            // the global object as well as generate our own custom start
            // function.
            // `document.currentScript` property can be null in browser extensions
            ImportKind::NoModules => {
                js.push_str("const __exports = {};\n");
                js.push_str("let script_src;\n");
                js.push_str(
                        "\
                        if (typeof document !== 'undefined' && document.currentScript !== null) {
                            script_src = new URL(document.currentScript.src, location.href).toString();
                        }\n",
                    );
                js.push_str("let wasm = undefined;\n");
                init = cx.gen_init(needs_manual_start, None)?;
                footer.push_str(&format!(
                    "{} = Object.assign(__wbg_init, {{ initSync }}, __exports);\n",
                    self.name.as_deref().unwrap_or(name)
                ));
            }

            // With normal CommonJS node we need to defer requiring the wasm
            // until the end so most of our own exports are hooked up
            ImportKind::Node if self.module_kind == ModuleKind::CommonJS => {
                js.push_str(&cx.generate_node_imports());
                js.push_str("let wasm;\n");

                for (id, js) in crate::sorted_iter(&cx.wasm_import_definitions) {
                    let import = cx.module.imports.get_mut(*id);
                    footer.push_str("\nmodule.exports.");
                    footer.push_str(&import.name);
                    footer.push_str(" = ");
                    footer.push_str(js.trim());
                    footer.push_str(";\n");
                }

                footer.push_str(
                    &cx.generate_node_wasm_loading(Path::new(&format!("./{}_bg.wasm", name))),
                );

                if needs_manual_start {
                    footer.push_str("\nwasm.__wbindgen_start();\n");
                }
            }

            // With Deno we need use the `Deno` namespace to load the wasm file
            ImportKind::Deno => {
                let (js_imports, wasm_import_object) = cx.generate_deno_imports();
                imports.push_str(&js_imports);
                footer.push_str(&wasm_import_object);

                footer.push_str(&cx.generate_deno_wasm_loading(module_name));

                footer.push_str("\n\n");

                if needs_manual_start {
                    footer.push_str("\nwasm.__wbindgen_start();\n");
                }
            }

            // With a browser-native output we're generating an ES module, but
            // browsers don't support natively importing wasm right now so we
            // expose the same initialization function as `--target no-modules`
            // as the default export of the module.
            ImportKind::Web => {
                cx.imports_post.push_str("let wasm;\n");
                init = cx.gen_init(needs_manual_start, Some(&mut imports))?;
                footer.push_str("export { initSync };\n");
                footer.push_str("export default __wbg_init;");
            }

            // With Bundlers we can simply import the wasm file as if it were an ES module
            // and let the bundler/runtime take care of it.
            // With Node we manually read the wasm file from the filesystem and instantiate it.
            _ => {
                for (id, js) in crate::sorted_iter(&cx.wasm_import_definitions) {
                    let import = cx.module.imports.get_mut(*id);
                    import.module = format!("./{}_bg.js", module_name);
                    if let Some(body) = js.strip_prefix("function") {
                        footer.push_str("\nexport function ");
                        footer.push_str(&import.name);
                        footer.push_str(body.trim());
                        footer.push_str(";\n");
                    } else {
                        footer.push_str("\nexport const ");
                        footer.push_str(&import.name);
                        footer.push_str(" = ");
                        footer.push_str(js.trim());
                        footer.push_str(";\n");
                    }
                }

                cx.imports_post.push_str(
                    "\
                        let wasm;
                        export function __wbg_set_wasm(val) {
                            wasm = val;
                        }
                        ",
                );

                if matches!(cx.config.mode, Preset::Node { module: true }) {
                    let start = start.get_or_insert_with(String::new);
                    start.push_str(&cx.generate_node_imports());
                    start.push_str(&cx.generate_node_wasm_loading(Path::new(&format!(
                        "./{}_bg.wasm",
                        module_name
                    ))));
                }
                if needs_manual_start {
                    start
                        .get_or_insert_with(String::new)
                        .push_str("\nwasm.__wbindgen_start();\n");
                }
            }
        }

        // Before putting the static init code declaration info, put all existing typescript into a `wasm_bindgen` namespace declaration.
        // Not sure if this should happen in all cases, so just adding it to NoModules for now...
        if self.config.mode.no_modules() {
            ts = String::from("declare namespace wasm_bindgen {\n\t");
            ts.push_str(&cx.typescript.replace('\n', "\n\t"));
            ts.push_str("\n}\n");
        } else {
            ts = cx.typescript.clone();
        }

        let (init_js, init_ts) = init;

        ts.push_str(&init_ts);

        // Emit all the JS for importing all our functionality
        assert!(
            !self.config.mode.uses_es_modules() || js.is_empty(),
            "ES modules require imports to be at the start of the file, but we \
                 generated some JS before the imports: {}",
            js
        );

        let mut push_with_newline = |s| {
            js.push_str(s);
            if !s.is_empty() {
                js.push('\n');
            }
        };

        push_with_newline(&imports);
        push_with_newline(&self.imports_post);

        // Emit all our exports from this module
        push_with_newline(&self.globals);

        // Generate the initialization glue, if there was any
        push_with_newline(&init_js);
        push_with_newline(&footer);
        if self.config.mode.no_modules() {
            js.push_str("})();\n");
        }

        while js.contains("\n\n\n") {
            js = js.replace("\n\n\n", "\n\n");
        }

        todo!();
    }
}
