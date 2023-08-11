use std::{
    collections::{HashSet, VecDeque},
    io::Write,
};

use anyhow::{bail, Result};
use indexmap::IndexMap;
use indoc::writedoc;
use next_core::{
    next_manifests::{ActionManifestWorkerEntry, ServerReferenceManifest},
    util::NextRuntime,
};
use next_swc::server_actions::parse_server_actions;
use turbo_tasks::{Value, ValueToString, Vc};
use turbopack_binding::{
    turbo::tasks_fs::{rope::RopeBuilder, File, FileSystemPath},
    turbopack::{
        core::{
            asset::AssetContent, chunk::EvaluatableAsset, context::AssetContext, module::Module,
            output::OutputAsset, reference::all_referenced_modules, reference_type::ReferenceType,
            virtual_output::VirtualOutputAsset, virtual_source::VirtualSource,
        },
        ecmascript::{
            chunk::{EcmascriptChunkItemExt, EcmascriptChunkPlaceable, EcmascriptChunkingContext},
            parse::ParseResult,
            EcmascriptModuleAsset,
        },
    },
};

async fn build_server_actions_loader(
    node_root: Vc<FileSystemPath>,
    original_name: &str,
    actions: Vc<ModuleActionMap>,
    asset_context: Vc<Box<dyn AssetContext>>,
) -> Result<Vc<Box<dyn EcmascriptChunkPlaceable>>> {
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

    let Some(placeable) =
        Vc::try_resolve_sidecast::<Box<dyn EcmascriptChunkPlaceable>>(module).await?
    else {
        bail!("internal module must be evaluatable");
    };

    Ok(placeable)
}

pub(crate) async fn create_server_actions_manifest(
    entry: Vc<Box<dyn EcmascriptChunkPlaceable>>,
    node_root: Vc<FileSystemPath>,
    original_name: &str,
    runtime: NextRuntime,
    asset_context: Vc<Box<dyn AssetContext>>,
    chunking_context: Vc<Box<dyn EcmascriptChunkingContext>>,
) -> Result<(
    Option<Vc<Box<dyn EvaluatableAsset>>>,
    Vc<Box<dyn OutputAsset>>,
)> {
    let actions = get_actions(Vc::upcast(entry));
    let actions_value = actions.await?;

    let path = node_root.join(format!(
        "server/app{original_name}/server-reference-manifest.json",
    ));
    let mut manifest = ServerReferenceManifest {
        ..Default::default()
    };

    if actions_value.is_empty() {
        let manifest = Vc::upcast(VirtualOutputAsset::new(
            path,
            AssetContent::file(File::from(serde_json::to_string_pretty(&manifest)?).into()),
        ));
        return Ok((None, manifest));
    }

    let mapping = match runtime {
        NextRuntime::Edge => &mut manifest.edge,
        NextRuntime::NodeJs => &mut manifest.node,
    };

    let loader =
        build_server_actions_loader(node_root, original_name, actions, asset_context).await?;
    let chunk_item_id = loader
        .as_chunk_item(chunking_context)
        .id()
        .to_string()
        .await?;

    for value in actions_value.values() {
        let value = value.await?;
        for hash in value.keys() {
            let entry = mapping.entry(hash.clone()).or_default();
            entry.workers.insert(
                format!("app{original_name}"),
                ActionManifestWorkerEntry::String(chunk_item_id.clone_value()),
            );
        }
    }
    let manifest = Vc::upcast(VirtualOutputAsset::new(
        path,
        AssetContent::file(File::from(serde_json::to_string_pretty(&manifest)?).into()),
    ));

    let Some(evaluable) = Vc::try_resolve_sidecast::<Box<dyn EvaluatableAsset>>(loader).await?
    else {
        bail!("loader module must be evaluatable");
    };

    Ok((Some(evaluable), manifest))
}

/// Finds the first page component in our loader tree, which should be the page
/// we're currently rendering.
#[turbo_tasks::function]
async fn get_actions(module: Vc<Box<dyn Module>>) -> Result<Vc<ModuleActionMap>> {
    let mut all_actions = IndexMap::new();

    let mut queue = VecDeque::from([module]);
    let mut seen = HashSet::new();
    while let Some(module) = queue.pop_front() {
        if let Some(actions) = &*parse_actions(module).await? {
            all_actions.insert(module, *actions);
        };

        // TODO: traversal graph
        let others = all_referenced_modules(module).await?;
        queue.extend(others.iter().filter(|m| seen.insert(**m)).cloned());
    }

    Ok(Vc::cell(all_actions))
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
struct ModuleActionMap(IndexMap<Vc<Box<dyn Module>>, Vc<ActionMap>>);

/// Maps the hashed `(filename, exported_action_name) -> exported_action_name`,
/// so that we can invoke the correct action function when we receive a request
/// with the hash in `Next-Action` header.
#[turbo_tasks::value(transparent)]
struct ActionMap(IndexMap<String, String>);

#[turbo_tasks::value(transparent)]
struct OptionActionMap(Option<Vc<ActionMap>>);

#[turbo_tasks::value_impl]
impl OptionActionMap {
    #[turbo_tasks::function]
    pub fn none() -> Vc<Self> {
        Vc::cell(None)
    }
}
