// src/lib.rs
//
// WASM-compatible library entry point.
// All filesystem I/O is handled in main.rs (native only).
// This file contains the core orchestration logic shared between
// the native binary and the WASM build.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

pub mod diagnostics;
pub mod dirgraph;
pub mod dirgraphsvg;
pub mod file_utils;
pub mod gsn;
pub mod outputs;
pub mod render;
pub mod yaml_fix;

use anyhow::{Result, anyhow};
use diagnostics::Diagnostics;
use dirgraphsvg::escape_text;
use gsn::{GsnDocument, GsnNode, Module, ModuleInformation, Origin};
use render::{RenderLegend, RenderOptions};
use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt::Display;
use std::error::Error;

#[cfg(target_arch = "wasm32")]
use console_error_panic_hook;

pub const MODULE_INFORMATION_NODE: &str = "module";

// ─── WASM public API ────────────────────────────────────────────────────────

/// Process a single GSN YAML document and return SVG as a string.
/// This is the main entry point for WASM consumers.
///
/// `yaml_input`  : contents of a .gsn.yaml file as a UTF-8 string
/// `char_wrap`   : optional character wrap width (0 = use default)
///
/// Returns the rendered SVG string, or an error message.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub fn process_gsn(yaml_input: &str, char_wrap: u32) -> Result<String, String> {
    let wrap = if char_wrap == 0 { None } else { Some(char_wrap) };
    process_gsn_internal(yaml_input, wrap).map_err(|e| e.to_string())
}

pub fn make_unknown_node_for(node_name: &str) -> GsnNode {
    let mut gsn_node = GsnNode {
        module: "Unknown".to_owned(),
        ..Default::default()
    };
    gsn_node.fix_node_type(node_name);
    gsn_node
}

#[cfg(feature = "wasm")]
#[wasm_bindgen(start)]
pub fn wasm_main() {
    console_error_panic_hook::set_once();
}


fn process_gsn_internal(yaml_input: &str, char_wrap: Option<u32>) -> Result<String> {
    let mut diags = Diagnostics::default();
    let mut nodes = BTreeMap::<String, GsnNode>::new();
    let mut modules = BTreeMap::<String, Module>::new();

    // Parse the YAML from a string (no filesystem)
    parse_yaml_str(
        yaml_input,
        "input.gsn.yaml",   // synthetic filename - used only for module naming
        &mut nodes,
        &mut modules,
        &mut diags,
    )?;

    // Validate
    validate_and_check(
        &mut nodes,
        &modules,
        &mut diags,
        &[],    // no excluded modules
        &[],    // no layers
        false,  // no extended check
        false,  // no dialectic warning
    ).map_err(|e| {
        // Collect diagnostics into the error message
        let msgs: String = diags.messages.iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        anyhow!("{e}\n{msgs}")
    })?;

    // After validate_and_check, before render — check all node_types are known   - Added by JS to improve error message utility
    let unknown_nodes: Vec<&str> = nodes.iter()
        .filter(|(_, n)| n.node_type.is_none())
        .map(|(id, _)| id.as_str())
        .collect();
    if !unknown_nodes.is_empty() {
        return Err(anyhow!(
            "Unknown node type(s): {}. Node names must start with: \
            G (Goal), S (Strategy), Sn (Solution), C (Context), \
             A (Assumption), J (Justification), CG (CounterGoal), CSn (CounterSolution).",
            unknown_nodes.join(", ")
        ));
    }

    // Build minimal RenderOptions — no clap, no filesystem paths
    let render_options = RenderOptions {
        stylesheets: vec![],
        masked_elements: vec![],
        layers: vec![],
        legend: RenderLegend::No,
        embed_stylesheets: false,
        architecture_filename: None,
        evidence_filename: None,
        complete_filename: None,
        output_directory: ".",
        skip_argument: true,
        char_wrap,
    };

    // Render complete view to an in-memory buffer
    let mut output: Vec<u8> = Vec::new();
    render::render_complete(&mut output, &nodes, &render_options)?;

    String::from_utf8(output).map_err(|e| anyhow!("SVG output was not valid UTF-8: {e}"))
}

// ─── Shared parsing logic (used by both lib.rs and main.rs) ─────────────────

/// Parse a YAML string into nodes and modules.
/// `filename` is a synthetic name used for module identification when
/// no `module:` key is present in the YAML.
pub fn parse_yaml_str(
    yaml_input: &str,
    filename: &str,
    nodes: &mut BTreeMap<String, GsnNode>,
    modules: &mut BTreeMap<String, Module>,
    diags: &mut Diagnostics,
) -> Result<()> {
    let mut n: BTreeMap<String, GsnDocument> = serde_yaml_ng::from_str(yaml_input)
        .map(|n: yaml_fix::YamlFixMap<String, GsnDocument>| n.into_inner())
        .map_err(|e| {
            anyhow!(
                "No valid GSN element can be found starting from line {}.\n\
                 This typically means that the YAML is completely invalid or \n\
                 the `text:` attribute is missing for an element.\n\
                 Please see the documentation for details.\n\
                 Original error message: {}.",
                e.location()
                    .map(|e| e.line().to_string())
                    .unwrap_or_else(|| "unknown".to_owned()),
                e
            )
        })?;

    let meta: ModuleInformation = match n.remove_entry(MODULE_INFORMATION_NODE) {
        Some((_, GsnDocument::ModuleInformation(x))) => x,
        _ => {
            let module_name = escape_text(&filename.to_owned());
            ModuleInformation::new(module_name)
        }
    };

    let module = meta.name.to_owned();

    // In WASM there is no real filesystem, so canonical_path is None.
    // The duplicate-module check based on canonical_path is skipped.
    match modules.entry(module.to_owned()) {
        Entry::Vacant(e) => {
            e.insert(Module {
                orig_file_name: filename.to_owned(),
                meta: meta.clone(),
                origin: Origin::CommandLine,
                canonical_path: None,
                output_path: None,
            });
            check_and_add_nodes(n, nodes, &module, diags, &filename.to_owned(), meta.char_wrap);
        }
        Entry::Occupied(e) => {
            diags.add_error(
                Some(&module),
                format!(
                    "C06: Module {} was already present in {}.",
                    filename,
                    e.get().orig_file_name,
                ),
            );
        }
    }

    Ok(())
}

// ─── Shared internal helpers (pub so main.rs can call them) ─────────────────

#[derive(PartialEq, Debug)]
pub struct ValidationOrCheckError {}

impl Display for ValidationOrCheckError {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unreachable!()
    }
}

impl Error for ValidationOrCheckError {}

pub fn check_and_add_nodes(
    mut n: BTreeMap<String, GsnDocument>,
    nodes: &mut BTreeMap<String, GsnNode>,
    module: &String,
    diags: &mut Diagnostics,
    input: &String,
    char_wrap: Option<u32>,
) {
    let node_names: Vec<String> = n.keys().cloned().collect();
    for node_name in node_names {
        if let Some((k, v)) = n.remove_entry(&node_name) {
            match nodes.entry(k.to_owned()) {
                Entry::Vacant(e) => match v {
                    GsnDocument::GsnNode(mut x) => {
                        module.clone_into(&mut x.module);
                        x.fix_node_type(&k);
                        x.supported_by.sort();
                        x.in_context_of.sort();
                        if x.char_wrap.is_none() {
                            x.char_wrap = char_wrap;
                        }
                        e.insert(x);
                    }
                    _ => unreachable!(),
                },
                Entry::Occupied(e) => {
                    diags.add_error(
                        Some(module),
                        format!(
                            "C07: Element {} in {} was already present in {}.",
                            k,
                            input,
                            e.get().module,
                        ),
                    );
                    break;
                }
            }
        }
    }
}

pub fn validate_and_check(
    nodes: &mut BTreeMap<String, GsnNode>,
    modules: &BTreeMap<String, Module>,
    diags: &mut Diagnostics,
    excluded_modules: &[&str],
    layers: &[&str],
    extended_check: bool,
    warn_dialectic: bool,
) -> Result<()> {
    if nodes.is_empty() {
        diags.add_error(None, "No input elements are found.".to_owned());
        return Err(ValidationOrCheckError {}.into());
    }

    let empty_modules: Vec<_> = modules
        .keys()
        .filter(|m| nodes.values().filter(|n| &&n.module == m).count() == 0)
        .cloned()
        .collect();

    if !empty_modules.is_empty() {
        for empty_module in empty_modules {
            diags.add_error(
                Some(&empty_module),
                "The module does not contain elements.".to_owned(),
            );
        }
        return Err(ValidationOrCheckError {}.into());
    }

    let result = || -> Result<(), ()> {
        for module_info in modules.values() {
            gsn::validation::validate_module(
                diags,
                &module_info.meta.name,
                module_info,
                nodes,
                extended_check,
                warn_dialectic,
            )?;
        }
        gsn::extend_modules(diags, nodes, modules)?;
        gsn::check::check_nodes(diags, nodes, excluded_modules)?;
        gsn::check::check_layers(diags, nodes, layers)
    }();

    result.map_err(|_| ValidationOrCheckError {}.into())
}

pub fn output_messages(diags: &Diagnostics) -> Result<()> {
    for msg in &diags.messages {
        eprintln!("{msg}");
    }
    if diags.errors == 0 {
        if diags.warnings > 0 {
            eprintln!("Warning: {} warnings detected.", diags.warnings);
        }
        Ok(())
    } else {
        Err(anyhow!(
            "{} errors and {} warnings detected.",
            diags.errors,
            diags.warnings
        ))
    }
}
