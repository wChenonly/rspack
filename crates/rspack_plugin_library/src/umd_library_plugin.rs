use std::{borrow::Cow, hash::Hash};

use rspack_core::{
  rspack_sources::{ConcatSource, RawStringSource, SourceExt},
  ApplyContext, Chunk, ChunkUkey, Compilation, CompilationAdditionalChunkRuntimeRequirements,
  CompilationParams, CompilerCompilation, CompilerOptions, ExternalModule, ExternalRequest,
  FilenameTemplate, LibraryAuxiliaryComment, LibraryCustomUmdObject, LibraryName,
  LibraryNonUmdObject, LibraryOptions, LibraryType, PathData, Plugin, PluginContext,
  RuntimeGlobals, SourceType,
};
use rspack_error::{error, Result};
use rspack_hash::RspackHash;
use rspack_hook::{plugin, plugin_hook};
use rspack_plugin_javascript::{
  JavascriptModulesChunkHash, JavascriptModulesRender, JsPlugin, RenderSource,
};
use rspack_util::infallible::ResultInfallibleExt as _;

use crate::utils::{external_arguments, externals_dep_array, get_options_for_chunk};

const PLUGIN_NAME: &str = "rspack.UmdLibraryPlugin";

#[derive(Debug)]
struct UmdLibraryPluginParsed<'a> {
  names: Cow<'a, LibraryCustomUmdObject>,
  auxiliary_comment: Option<&'a LibraryAuxiliaryComment>,
  named_define: Option<bool>,
}

#[plugin]
#[derive(Debug)]
pub struct UmdLibraryPlugin {
  _optional_amd_external_as_global: bool,
  library_type: LibraryType,
}

impl UmdLibraryPlugin {
  pub fn new(optional_amd_external_as_global: bool, library_type: LibraryType) -> Self {
    Self::new_inner(optional_amd_external_as_global, library_type)
  }

  fn parse_options<'a>(&self, library: &'a LibraryOptions) -> UmdLibraryPluginParsed<'a> {
    let names = if let Some(LibraryName::UmdObject(names)) = &library.name {
      Cow::Borrowed(names)
    } else {
      let (single_name, root) = library
        .name
        .as_ref()
        .and_then(|n| match n {
          LibraryName::NonUmdObject(LibraryNonUmdObject::String(s)) => {
            Some((s.clone(), vec![s.clone()]))
          }
          LibraryName::NonUmdObject(LibraryNonUmdObject::Array(arr)) => {
            Some((arr.first()?.clone(), arr.clone()))
          }
          LibraryName::UmdObject(_) => unreachable!(),
        })
        .unzip();
      Cow::Owned(LibraryCustomUmdObject {
        commonjs: single_name.clone(),
        root,
        amd: single_name,
      })
    };
    UmdLibraryPluginParsed {
      names,
      auxiliary_comment: library.auxiliary_comment.as_ref(),
      named_define: library.umd_named_define,
    }
  }

  fn get_options_for_chunk<'a>(
    &self,
    compilation: &'a Compilation,
    chunk_ukey: &'a ChunkUkey,
  ) -> Option<UmdLibraryPluginParsed<'a>> {
    get_options_for_chunk(compilation, chunk_ukey)
      .filter(|library| library.library_type == self.library_type)
      .map(|library| self.parse_options(library))
  }
}

#[plugin_hook(CompilerCompilation for UmdLibraryPlugin)]
async fn compilation(
  &self,
  compilation: &mut Compilation,
  _params: &mut CompilationParams,
) -> Result<()> {
  let mut hooks = JsPlugin::get_compilation_hooks_mut(compilation);
  hooks.render.tap(render::new(self));
  hooks.chunk_hash.tap(js_chunk_hash::new(self));
  Ok(())
}

#[plugin_hook(JavascriptModulesRender for UmdLibraryPlugin)]
fn render(
  &self,
  compilation: &Compilation,
  chunk_ukey: &ChunkUkey,
  render_source: &mut RenderSource,
) -> Result<()> {
  let Some(options) = self.get_options_for_chunk(compilation, chunk_ukey) else {
    return Ok(());
  };
  let supports_arrow_function = compilation
    .options
    .output
    .environment
    .supports_arrow_function();
  let chunk = compilation.chunk_by_ukey.expect_get(chunk_ukey);
  let module_graph = compilation.get_module_graph();
  let modules = compilation
    .chunk_graph
    .get_chunk_modules_identifier(chunk_ukey)
    .iter()
    .filter_map(|identifier| {
      module_graph
        .module_by_identifier(identifier)
        .and_then(|module| module.as_external_module())
        .and_then(|m| {
          let ty = m.get_external_type();
          (ty == "umd" || ty == "umd2").then_some(m)
        })
    })
    .collect::<Vec<&ExternalModule>>();
  // TODO check if external module is optional
  let optional_externals: Vec<&ExternalModule> = vec![];
  let externals = modules.clone();
  let required_externals = modules.clone();

  let amd_factory = if optional_externals.is_empty() {
    "factory"
  } else {
    ""
  };

  let UmdLibraryPluginParsed {
    names,
    auxiliary_comment,
    named_define,
  } = options;

  let define = if let (Some(amd), Some(_)) = &(&names.amd, named_define) {
    format!(
      "define({}, {}, {amd_factory});\n",
      library_name(&[amd.to_string()], chunk, compilation),
      externals_dep_array(&required_externals)?
    )
  } else {
    format!(
      "define({}, {amd_factory});\n",
      externals_dep_array(&required_externals)?
    )
  };

  let factory = if names.commonjs.is_some() || names.root.is_some() {
    let commonjs_code = format!(
      "{}
      exports[{}] = factory({});\n",
      get_auxiliary_comment("commonjs", auxiliary_comment),
      names
        .commonjs
        .clone()
        .map(|commonjs| library_name(&[commonjs], chunk, compilation))
        .or_else(|| names
          .root
          .clone()
          .map(|root| library_name(&root, chunk, compilation)))
        .unwrap_or_default(),
      externals_require_array("commonjs", &externals)?,
    );
    let root_code = format!(
      "{}
      {} = factory({});",
      get_auxiliary_comment("root", auxiliary_comment),
      replace_keys(
        accessor_access(
          Some("root"),
          &names
            .root
            .clone()
            .or_else(|| names.commonjs.clone().map(|commonjs| vec![commonjs]))
            .unwrap_or_default(),
        ),
        chunk,
        compilation,
      ),
      externals_root_array(&externals)?
    );
    format!(
      "}} else if(typeof exports === 'object'){{\n
          {commonjs_code}
      }} else {{\n
          {root_code}
      }}\n",
    )
  } else {
    let value = if externals.is_empty() {
      "var a = factory();\n".to_string()
    } else {
      format!(
        "var a = typeof exports === 'object' ? factory({}) : factory({});\n",
        externals_require_array("commonjs", &externals)?,
        externals_root_array(&externals)?
      )
    };
    format!(
      "}} else {{
          {value}
          for(var i in a) (typeof exports === 'object' ? exports : root)[i] = a[i];\n
      }}\n"
    )
  };

  let mut source = ConcatSource::default();
  source.add(RawStringSource::from(
    "(function webpackUniversalModuleDefinition(root, factory) {\n",
  ));
  source.add(RawStringSource::from(format!(
    r#"{}
      if(typeof exports === 'object' && typeof module === 'object') {{
          module.exports = factory({});
      }}"#,
    get_auxiliary_comment("commonjs2", auxiliary_comment),
    externals_require_array("commonjs2", &externals)?
  )));
  source.add(RawStringSource::from(format!(
    "else if(typeof define === 'function' && define.amd) {{
          {}
          {define}
          {factory}
      }})({}, {} {{
          return ",
    get_auxiliary_comment("amd", auxiliary_comment),
    compilation.options.output.global_object,
    if supports_arrow_function {
      format!("({}) =>", external_arguments(&externals, compilation))
    } else {
      format!("function({})", external_arguments(&externals, compilation))
    },
  )));
  source.add(render_source.source.clone());
  source.add(RawStringSource::from_static("\n})"));
  render_source.source = source.boxed();
  Ok(())
}

#[plugin_hook(JavascriptModulesChunkHash for UmdLibraryPlugin)]
async fn js_chunk_hash(
  &self,
  compilation: &Compilation,
  chunk_ukey: &ChunkUkey,
  hasher: &mut RspackHash,
) -> Result<()> {
  let Some(_) = self.get_options_for_chunk(compilation, chunk_ukey) else {
    return Ok(());
  };
  PLUGIN_NAME.hash(hasher);
  compilation.options.output.library.hash(hasher);
  Ok(())
}

#[plugin_hook(CompilationAdditionalChunkRuntimeRequirements for UmdLibraryPlugin)]
fn additional_chunk_runtime_requirements(
  &self,
  compilation: &mut Compilation,
  chunk_ukey: &ChunkUkey,
  runtime_requirements: &mut RuntimeGlobals,
) -> Result<()> {
  let Some(_) = self.get_options_for_chunk(compilation, chunk_ukey) else {
    return Ok(());
  };
  runtime_requirements.insert(RuntimeGlobals::RETURN_EXPORTS_FROM_RUNTIME);
  Ok(())
}

#[async_trait::async_trait]
impl Plugin for UmdLibraryPlugin {
  fn name(&self) -> &'static str {
    PLUGIN_NAME
  }

  fn apply(&self, ctx: PluginContext<&mut ApplyContext>, _options: &CompilerOptions) -> Result<()> {
    ctx
      .context
      .compiler_hooks
      .compilation
      .tap(compilation::new(self));
    ctx
      .context
      .compilation_hooks
      .additional_chunk_runtime_requirements
      .tap(additional_chunk_runtime_requirements::new(self));
    Ok(())
  }
}

fn library_name(v: &[String], chunk: &Chunk, compilation: &Compilation) -> String {
  let value =
    serde_json::to_string(v.last().expect("should have last")).expect("invalid module_id");
  replace_keys(value, chunk, compilation)
}

fn replace_keys(v: String, chunk: &Chunk, compilation: &Compilation) -> String {
  compilation
    .get_path(
      &FilenameTemplate::from(v),
      PathData::default()
        .chunk_id_optional(
          chunk
            .id(&compilation.chunk_ids_artifact)
            .map(|id| id.as_str()),
        )
        .chunk_hash_optional(chunk.rendered_hash(
          &compilation.chunk_hashes_artifact,
          compilation.options.output.hash_digest_length,
        ))
        .chunk_name_optional(chunk.name_for_filename_template(&compilation.chunk_ids_artifact))
        .content_hash_optional(chunk.rendered_content_hash_by_source_type(
          &compilation.chunk_hashes_artifact,
          &SourceType::JavaScript,
          compilation.options.output.hash_digest_length,
        )),
    )
    .always_ok()
}

fn externals_require_array(typ: &str, externals: &[&ExternalModule]) -> Result<String> {
  Ok(
    externals
      .iter()
      .map(|m| {
        let request = match &m.request {
          ExternalRequest::Single(r) => r,
          ExternalRequest::Map(map) => map
            .get(typ)
            .ok_or_else(|| error!("Missing external configuration for type: {typ}"))?,
        };
        // TODO: check if external module is optional
        let primary =
          serde_json::to_string(request.primary()).map_err(|e| error!(e.to_string()))?;
        let expr = if let Some(rest) = request.rest() {
          format!("require({}){}", primary, &accessor_to_object_access(rest))
        } else {
          format!("require({})", primary)
        };
        Ok(expr)
      })
      .collect::<Result<Vec<_>>>()?
      .join(", "),
  )
}

fn externals_root_array(modules: &[&ExternalModule]) -> Result<String> {
  Ok(
    modules
      .iter()
      .map(|m| {
        let typ = "root";
        let request = match &m.request {
          ExternalRequest::Single(r) => r.iter(),
          ExternalRequest::Map(map) => map
            .get(typ)
            .map(|r| r.iter())
            .ok_or_else(|| error!("Missing external configuration for type: {typ}"))?,
        };
        Ok(format!("root{}", accessor_to_object_access(request)))
      })
      .collect::<Result<Vec<_>>>()?
      .join(", "),
  )
}

fn accessor_to_object_access<S: AsRef<str>>(accessor: impl IntoIterator<Item = S>) -> String {
  accessor
    .into_iter()
    .map(|s| {
      format!(
        "[{}]",
        serde_json::to_string(s.as_ref()).expect("failed to serde_json::to_string")
      )
    })
    .collect::<Vec<_>>()
    .join("")
}

fn accessor_access(base: Option<&str>, accessor: &[String]) -> String {
  accessor
    .iter()
    .enumerate()
    .map(|(i, _)| {
      let a = if let Some(base) = base {
        format!("{base}{}", accessor_to_object_access(&accessor[..(i + 1)]))
      } else {
        format!(
          "{}{}",
          accessor[0],
          accessor_to_object_access(&accessor[1..(i + 1)])
        )
      };
      if i == accessor.len() - 1 {
        return a;
      }
      if i == 0 && base.is_none() {
        return format!("{a} = typeof {a} === 'object' ? {a} : {{}}");
      }
      format!("{a} = {a} || {{}}")
    })
    .collect::<Vec<_>>()
    .join(", ")
}

fn get_auxiliary_comment(t: &str, auxiliary_comment: Option<&LibraryAuxiliaryComment>) -> String {
  if let Some(auxiliary_comment) = auxiliary_comment {
    if let Some(value) = match t {
      "amd" => &auxiliary_comment.amd,
      "commonjs" => &auxiliary_comment.commonjs,
      "commonjs2" => &auxiliary_comment.commonjs2,
      "root" => &auxiliary_comment.root,
      _ => &None,
    } {
      return format!("\t// {value} \n");
    }
  }
  "".to_string()
}
