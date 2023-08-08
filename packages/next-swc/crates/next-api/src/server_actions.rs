use std::{collections::VecDeque, io::Write};

use anyhow::{bail, Result};
use indexmap::IndexMap;
use indoc::writedoc;
use next_core::{
    app_structure::LoaderTree,
    next_manifests::{ActionManifestWorkerEntry, ServerReferenceManifest},
    util::NextRuntime,
};
use next_swc::server_actions::parse_server_actions;
use turbo_tasks::{debug::ValueDebug, Value, ValueToString, Vc};
use turbopack_binding::{
    turbo::tasks_fs::{rope::RopeBuilder, File, FileSystemPath},
    turbopack::{
        core::{
            asset::{Asset, AssetContent},
            chunk::{Chunk, ChunkableModule, ChunkingContext},
            context::AssetContext,
            file_source::FileSource,
            module::Module,
            output::{OutputAsset, OutputAssets},
            reference::all_referenced_modules,
            reference_type::{EcmaScriptModulesReferenceSubType, ReferenceType},
            source::Source,
            virtual_output::VirtualOutputAsset,
            virtual_source::VirtualSource,
        },
        ecmascript::{chunk::EcmascriptChunkPlaceable, parse::ParseResult, EcmascriptModuleAsset},
        turbopack::rebase::RebasedAsset,
    },
};

async fn build_server_actions_loader(
    node_root: Vc<FileSystemPath>,
    project_root: Vc<FileSystemPath>,
    original_name: &str,
    actions: Vc<ModuleActionMap>,
    asset_context: Vc<Box<dyn AssetContext>>,
    chunking_context: Vc<Box<dyn ChunkingContext>>,
) -> Result<Vc<Box<dyn Chunk>>> {
    let actions = actions.await?;

    let mut contents = RopeBuilder::from("__turbopack_export_value__({\n");
    let mut import_map = IndexMap::with_capacity(actions.len());
    for (i, (module, actions_map)) in actions.iter().enumerate() {
        for (id, name) in &*actions_map.await? {
            writedoc!(
                contents,
                "
    \x20 '{id}': (...args) => import('ACTIONS_MODULE{i}')
      .then(mod => (0, mod['{name}'])(...args)),\n
    ",
            )?;
        }
        import_map.insert(format!("ACTIONS_MODULE{i}"), *module);
    }
    write!(contents, "}});")?;

    let output_path = node_root.join(format!("server/app{original_name}/actions.js"));
    let file = File::from(contents.build());
    let source = VirtualSource::new(output_path, AssetContent::file(file.into()));
    let module = asset_context.process(
        Vc::upcast(source),
        Value::new(ReferenceType::Internal(Vc::cell(import_map))),
    );

    let Some(module) =
        Vc::try_resolve_sidecast::<Box<dyn EcmascriptChunkPlaceable>>(module).await?
    else {
        bail!("internal module must be evaluatable");
    };

    Ok(module.as_root_chunk(chunking_context))
}

pub(crate) async fn create_server_actions_manifest(
    node_root: Vc<FileSystemPath>,
    project_root: Vc<FileSystemPath>,
    original_name: &str,
    runtime: NextRuntime,
    actions: Vc<ModuleActionMap>,
    output_assets: &mut Vec<Vc<Box<dyn OutputAsset>>>,
    asset_context: Vc<Box<dyn AssetContext>>,
    chunking_context: Vc<Box<dyn ChunkingContext>>,
) -> Result<()> {
    let actions_value = actions.await?;
    if actions_value.is_empty() {
        return Ok(());
    }

    let path = node_root.join(format!(
        "server/app{original_name}/server-reference-manifest.json",
    ));
    let mut manifest = ServerReferenceManifest {
        ..Default::default()
    };
    let mapping = match runtime {
        NextRuntime::Edge => &mut manifest.edge,
        NextRuntime::NodeJs => &mut manifest.node,
    };

    let loader_chunk = build_server_actions_loader(
        node_root,
        project_root,
        original_name,
        actions,
        asset_context,
        chunking_context,
    )
    .await?;

    let loader_outputs = chunking_context.chunk_group(loader_chunk).await?;
    output_assets.extend(loader_outputs.iter().cloned());

    for value in actions_value.values() {
        let value = value.await?;
        for hash in value.keys() {
            let entry = mapping.entry(hash.clone()).or_default();
            entry.workers.insert(
                format!("app{original_name}"),
                ActionManifestWorkerEntry::String(
                    loader_chunk.ident().to_string().await?.clone_value(),
                ),
            );
        }
    }

    output_assets.push(Vc::upcast(VirtualOutputAsset::new(
        path,
        AssetContent::file(File::from(serde_json::to_string_pretty(&manifest)?).into()),
    )));
    Ok(())
}

/// Finds the first page component in our loader tree, which should be the page
/// we're currently rendering.
#[turbo_tasks::function]
pub(crate) async fn get_actions(
    loader_tree: Vc<LoaderTree>,
    context: Vc<Box<dyn AssetContext>>,
) -> Result<Vc<ModuleActionMap>> {
    let mut all_actions = IndexMap::new();

    let source: Vc<Box<dyn Source>> = Vc::upcast(FileSource::new(original_page_path(loader_tree)));
    // To avoid regex searching, we turn the file into it's SWC parse result and
    // iterate the leading comments.
    let module = context.process(
        source,
        turbo_tasks::Value::new(ReferenceType::EcmaScriptModules(
            EcmaScriptModulesReferenceSubType::Undefined,
        )),
    );

    let mut queue = VecDeque::from([module]);
    while let Some(module) = queue.pop_front() {
        if let Some(actions) = &*parse_actions(module).await? {
            all_actions.insert(module, *actions);
        };

        // TODO: traversal graph
        let others = all_referenced_modules(module).await?;
        queue.extend(others.iter().cloned());
    }

    Ok(Vc::cell(all_actions))
}

/// Finds the first page component in our loader tree, which should be the page
/// we're currently rendering.
#[turbo_tasks::function]
async fn original_page_path(tree: Vc<LoaderTree>) -> Result<Vc<FileSystemPath>> {
    let mut queue = VecDeque::new();
    queue.push_back(tree);
    // For some reason, the main LaoderTree doesn't have a page, and you need to
    // recursively traverse every parallel route looking for it.
    while let Some(tree) = queue.pop_front() {
        let tree_value = tree.await?;
        if let Some(page) = tree_value.components.await?.page {
            return Ok(page);
        }
        queue.extend(tree_value.parallel_routes.values().cloned());
    }
    bail!("could not locate component's source path")
}

#[turbo_tasks::function]
async fn parse_actions(module: Vc<Box<dyn Module>>) -> Result<Vc<OptionActionMap>> {
    let Some(ecmascript_asset) =
        Vc::try_resolve_downcast_type::<EcmascriptModuleAsset>(module).await?
    else {
        return Ok(OptionActionMap::none());
    };
    let ParseResult::Ok {
        comments, program, ..
    } = &*ecmascript_asset.parse().await?
    else {
        bail!("failed to parse action module")
    };

    let actions = parse_server_actions(&program, comments.clone());
    Ok(Vc::cell(actions.map(Vc::cell)))
}

#[turbo_tasks::value(transparent)]
pub(crate) struct ModuleActionMap(IndexMap<Vc<Box<dyn Module>>, Vc<ActionMap>>);

#[turbo_tasks::value_impl]
impl ModuleActionMap {
    #[turbo_tasks::function]
    pub fn empty() -> Vc<Self> {
        Vc::cell(IndexMap::new())
    }
}

/// Maps the hashed `(filename, exported_action_name) -> exported_action_name`,
/// so that we can invoke the correct action function when we receive a request
/// with the hash in `Next-Action` header.
#[turbo_tasks::value(transparent)]
pub(crate) struct ActionMap(IndexMap<String, String>);

#[turbo_tasks::value(transparent)]
pub(crate) struct OptionActionMap(Option<Vc<ActionMap>>);

#[turbo_tasks::value_impl]
impl OptionActionMap {
    #[turbo_tasks::function]
    pub fn none() -> Vc<Self> {
        Vc::cell(None)
    }
}
