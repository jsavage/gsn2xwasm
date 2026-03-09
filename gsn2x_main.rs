// src/main.rs
//
// Native CLI binary entry point.
// All core logic lives in lib.rs.
// This file handles filesystem I/O, clap arg parsing, and output paths —
// none of which are available in WASM.

use anyhow::{Context, Result, anyhow};
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, Command, value_parser};
use gsn2x_lib::file_utils::{create_file_incl_parent, translate_to_output_path};
use gsn2x_lib::render::RenderOptions;
use gsn2x_lib::{
    MODULE_INFORMATION_NODE,
    ValidationOrCheckError,
    check_and_add_nodes,
    validate_and_check,
    output_messages,
};
use std::collections::BTreeMap;
use std::io::{BufReader, stdout};
use std::path::{Path, PathBuf};
use std::{collections::btree_map::Entry, fs::File};

use gsn2x_lib::diagnostics::Diagnostics;
use gsn2x_lib::dirgraphsvg::escape_text;
use gsn2x_lib::gsn::{FindModuleByPath, GsnDocument, GsnNode, Module, ModuleInformation, Origin};

fn main() -> Result<()> {
    let mut command = build_command_options();
    let matches = command.clone().get_matches();

    let mut diags = Diagnostics::default();

    let inputs: Vec<String> = matches
        .get_many::<String>("INPUT")
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let layers = matches
        .get_many::<String>("LAYERS")
        .into_iter()
        .flatten()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>();
    let excluded_modules = matches
        .get_many::<String>("EXCLUDED_MODULE")
        .into_iter()
        .flatten()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>();

    if matches.value_source("INPUT") == Some(ValueSource::DefaultValue)
        && !Path::new(inputs.first().unwrap()).exists()
    {
        command.print_help()?;
        Err(anyhow!("index.gsn.yaml not found."))
    } else {
        let output_directory = matches.get_one::<String>("OUTPUT_DIRECTORY").unwrap();

        let mut nodes = BTreeMap::<String, GsnNode>::new();
        let mut modules: BTreeMap<String, Module> = BTreeMap::new();

        let read_and_check = || -> Result<()> {
            read_inputs(
                &inputs,
                &mut nodes,
                &mut modules,
                &mut diags,
                output_directory,
            )?;
            validate_and_check(
                &mut nodes,
                &modules,
                &mut diags,
                &excluded_modules,
                &layers,
                matches.get_flag("EXTENDED_CHECK"),
                matches.get_flag("WARN_DIALECTIC"),
            )
        }();

        match read_and_check {
            Err(e) if e.is::<ValidationOrCheckError>() => Ok(()),
            Err(e) => Err(e),
            Ok(_) => {
                if !matches.get_flag("CHECK_ONLY") {
                    if !std::path::Path::new(&output_directory).exists() {
                        std::fs::create_dir_all(output_directory).with_context(|| {
                            format!("Could not create output directory {output_directory}")
                        })?;
                    }
                    let embed_stylesheets = matches.get_flag("EMBED_CSS");
                    let mut stylesheets = matches
                        .get_many::<String>("STYLESHEETS")
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect::<Vec<_>>();
                    stylesheets.append(
                        &mut modules
                            .iter()
                            .flat_map(|m| m.1.meta.stylesheets.to_owned())
                            .collect::<Vec<_>>(),
                    );
                    copy_and_prepare_stylesheets(
                        &mut stylesheets,
                        embed_stylesheets,
                        output_directory,
                    )?;
                    let mut render_options = RenderOptions::new(
                        &matches,
                        stylesheets,
                        embed_stylesheets,
                        output_directory,
                    );
                    add_missing_nodes_and_modules(&mut nodes, &mut modules, &mut render_options);
                    print_outputs(&nodes, &modules, &render_options)?;
                }

                if let Some(ValueSource::CommandLine) = matches.value_source("STATISTICS") {
                    let mut output = match matches.get_one::<String>("STATISTICS") {
                        Some(path) => Box::new(File::create(path)?) as Box<dyn std::io::Write>,
                        None => Box::new(stdout().lock()) as Box<dyn std::io::Write>,
                    };
                    gsn2x_lib::outputs::render_statistics(&mut output, &nodes, &modules)?;
                }

                if let Some(ValueSource::CommandLine) = matches.value_source("YAMLDUMP") {
                    let mut output = match matches.get_one::<String>("YAMLDUMP") {
                        Some(path) => create_file_incl_parent(Path::new(path))?,
                        None => Box::new(stdout().lock()) as Box<dyn std::io::Write>,
                    };
                    gsn2x_lib::outputs::render_yaml_docs(&mut output, &nodes, &modules)?;
                }
                Ok(())
            }
        }?;

        output_messages(&diags)
    }
}

fn add_missing_nodes_and_modules(
    nodes: &mut BTreeMap<String, GsnNode>,
    modules: &mut BTreeMap<String, Module>,
    render_options: &mut RenderOptions,
) {
    let mut add_nodes = vec![];
    for (_, node) in nodes.iter() {
        let ref_nodes: Vec<_> = node
            .supported_by
            .iter()
            .chain(node.in_context_of.iter())
            .collect();
        for ref_node in ref_nodes {
            if !nodes.contains_key(ref_node) {
                add_nodes.push(ref_node.to_owned());
            }
        }
    }
    for node in add_nodes {
        let mut gsn_node = GsnNode {
            module: "Unknown".to_owned(),
            ..Default::default()
        };
        gsn_node.fix_node_type(&node);
        nodes.insert(node.to_owned(), gsn_node);
        render_options.masked_elements.push(node);
    }
    let _ = modules.insert(
        "Unknown".to_owned(),
        Module {
            orig_file_name: "".to_owned(),
            meta: ModuleInformation::new("Unknown".to_owned()),
            origin: Origin::Excluded,
            canonical_path: None,
            output_path: None,
        },
    );
}

fn read_inputs(
    inputs: &[String],
    nodes: &mut BTreeMap<String, GsnNode>,
    modules: &mut BTreeMap<String, Module>,
    diags: &mut Diagnostics,
    output_directory: &str,
) -> Result<()> {
    let mut copied_inputs: Vec<String> = inputs.iter().map(|i| i.replace('\\', "/")).collect();
    let mut first_run = true;
    'outer: loop {
        let mut additional_inputs = vec![];
        for input in &copied_inputs {
            let reader =
                BufReader::new(File::open(input).context(format!("Failed to open file {input}"))?);

            let mut n: BTreeMap<String, GsnDocument> = serde_yaml_ng::from_reader(reader)
                .map(|n: gsn2x_lib::yaml_fix::YamlFixMap<String, GsnDocument>| n.into_inner())
                .map_err(|e| {
                    anyhow!(format!(
                        "No valid GSN element can be found starting from line {}.\n\
                         This typically means that the YAML is completely invalid or \n\
                         the `text:` attribute is missing for an element.\n\
                         Please see the documentation for details.\n\
                         Original error message: {}.",
                        e.location()
                            .map(|e| e.line().to_string())
                            .unwrap_or_else(|| "unknown".to_owned()),
                        e
                    ))
                })
                .context(format!("Failed to parse YAML from file {input}"))?;

            let meta: ModuleInformation = match n.remove_entry(MODULE_INFORMATION_NODE) {
                Some((_, GsnDocument::ModuleInformation(x))) => x,
                _ => {
                    let module_name = escape_text(&input.to_owned());
                    ModuleInformation::new(module_name)
                }
            };

            let module = meta.name.to_owned();
            let pb = PathBuf::from(input)
                .canonicalize()
                .with_context(|| format!("Failed to open file {input}."))?;
            let module_name_exists = modules.find_module_by_path(&pb).is_some();

            match modules.entry(module.to_owned()) {
                Entry::Vacant(e) if !module_name_exists => {
                    e.insert(Module {
                        orig_file_name: input.to_owned(),
                        meta: meta.clone(),
                        origin: if first_run {
                            Origin::CommandLine
                        } else {
                            Origin::File(input.to_owned())
                        },
                        canonical_path: Some(pb),
                        output_path: translate_to_output_path(output_directory, input, Some("svg"))
                            .ok(),
                    });
                    check_and_add_nodes(n, nodes, &module, diags, &input.to_owned(), meta.char_wrap);
                    let imported_files = get_uses_files(&meta, input, diags);
                    additional_inputs.extend(imported_files.to_vec());
                }
                Entry::Vacant(_) => unreachable!(),
                Entry::Occupied(e) => {
                    diags.add_error(
                        Some(&module),
                        format!(
                            "C06: Module in {} was already present in {} provided by {}.",
                            input,
                            e.get().orig_file_name,
                            e.get().origin,
                        ),
                    );
                    break 'outer Err(ValidationOrCheckError {}.into());
                }
            }
        }
        if additional_inputs.is_empty() {
            break Ok(());
        } else {
            copied_inputs.clear();
            copied_inputs.append(&mut additional_inputs);
        }
        first_run = false;
    }
}

fn get_uses_files(
    meta: &ModuleInformation,
    input: &String,
    diags: &mut Diagnostics,
) -> Vec<String> {
    meta.uses
        .iter()
        .filter_map(|r| match PathBuf::from(r) {
            x if x.is_relative() => PathBuf::from(input).parent().map(|p| {
                let mut new_r = p.to_path_buf();
                new_r.push(r);
                new_r.to_string_lossy().to_string()
            }),
            x if x.is_absolute() => Some(r.to_owned()),
            _ => {
                diags.add_warning(
                    Some(&meta.name),
                    format!("Could not identify used file {r} in module; ignoring it."),
                );
                None
            }
        })
        .map(|i| i.replace('\\', "/"))
        .collect()
}

fn print_outputs(
    nodes: &BTreeMap<String, GsnNode>,
    modules: &BTreeMap<String, Module>,
    render_options: &RenderOptions,
) -> Result<()> {
    let output_path = render_options.output_directory.to_owned();
    if !render_options.skip_argument {
        for (_, module) in modules.iter().filter(|(m, _)| *m != "Unknown") {
            let output_path = Path::new(module.output_path.as_ref().unwrap());
            let mut output_file = create_file_incl_parent(output_path)?;
            print!("Rendering \"{}\": ", output_path.display());
            gsn2x_lib::render::render_argument(
                &mut output_file,
                &module.meta.name,
                modules,
                nodes,
                render_options,
            )?;
        }
    }
    if modules.iter().filter(|(m, _)| *m != "Unknown").count() > 1 {
        if let Some(architecture_filename) = &render_options.architecture_filename {
            let arch_output_path =
                translate_to_output_path(&output_path, architecture_filename, None)?;
            let mut output_file = File::create(&arch_output_path)
                .context(format!("Failed to open output file {arch_output_path}"))?;
            let dependencies = gsn2x_lib::gsn::calculate_module_dependencies(nodes);
            print!("Rendering \"{arch_output_path}\": ");
            gsn2x_lib::render::render_architecture(
                &mut output_file,
                modules,
                dependencies,
                render_options,
                &arch_output_path,
            )?;
        }
        if let Some(complete_filename) = &render_options.complete_filename {
            let output_path = translate_to_output_path(&output_path, complete_filename, None)?;
            let mut output_file = File::create(&output_path)
                .context(format!("Failed to open output file {output_path}"))?;
            print!("Rendering \"{output_path}\": ");
            gsn2x_lib::render::render_complete(&mut output_file, nodes, render_options)?;
        }
    }
    if let Some(evidence_filename) = &render_options.evidence_filename {
        let output_path = translate_to_output_path(&output_path, evidence_filename, None)?;
        let mut output_file = File::create(&output_path)
            .context(format!("Failed to open output file {output_path}"))?;
        print!("Writing evidence \"{output_path}\": ");
        gsn2x_lib::outputs::render_evidence(&mut output_file, nodes, render_options)?;
    }
    Ok(())
}

pub(crate) fn copy_and_prepare_stylesheets(
    stylesheets: &mut [String],
    embed_stylesheets: bool,
    output_directory: &str,
) -> Result<()> {
    for stylesheet in stylesheets {
        let new_name = if gsn2x_lib::file_utils::is_url(stylesheet) {
            format!("url({stylesheet})")
        } else if embed_stylesheets {
            stylesheet.to_owned()
        } else {
            let css_path = PathBuf::from(&stylesheet).canonicalize()?;
            let mut out_path = PathBuf::from(output_directory).canonicalize()?;
            out_path.push(css_path.file_name().ok_or(anyhow!(
                "Could not identify stylesheet filename in {}",
                stylesheet
            ))?);
            if css_path != out_path {
                std::fs::copy(&css_path, &out_path).with_context(|| {
                    format!(
                        "Could not copy stylesheet from {} to {}",
                        css_path.display(),
                        &out_path.display()
                    )
                })?;
            }
            out_path.to_string_lossy().to_string()
        };
        new_name.clone_into(stylesheet);
    }
    Ok(())
}
