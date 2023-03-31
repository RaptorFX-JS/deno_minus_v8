use crate::error::generic_error;
use anyhow::Error;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub type ModuleId = i32;

// minus_v8: since we don't do module loading this is not API-compatible with vanilla Deno
#[derive(Default)]
pub(crate) struct ModuleMap {
  pub next_module_id: ModuleId,
  pub by_id: HashMap<ModuleId, ModuleInfo>,
}

// minus_v8: also not API-compatible with vanilla Deno
pub(crate) struct ModuleInfo {
  pub id: ModuleId,
  pub main: bool,
  pub name: String,
}

impl ModuleMap {
  // minus_v8: internal impl detail
  fn next_module_id(module_map_rc: Rc<RefCell<ModuleMap>>) -> ModuleId {
    let mut module_map = (*module_map_rc).borrow_mut();
    let id = module_map.next_module_id;
    module_map.next_module_id += 1;
    id
  }

  pub(crate) async fn load_main(
    module_map_rc: Rc<RefCell<ModuleMap>>,
    specifier: &str,
  ) -> Result<ModuleId, Error> {
    {
      let module_map = module_map_rc.borrow();
      let maybe_main_module = module_map.by_id.values().find(|module| module.main);
      if let Some(main_module) = maybe_main_module {
        return Err(generic_error(
          format!("Trying to create \"main\" module ({:?}), when one already exists ({:?})",
                  specifier,
                  main_module.name,
          )));
      }
    }

    let id = ModuleMap::next_module_id(module_map_rc.clone());
    let mut module_map = (*module_map_rc).borrow_mut();
    let info = ModuleInfo {
      id,
      main: true,
      name: specifier.to_string(),
    };
    module_map.by_id.insert(id, info);
    Ok(id)
  }

  pub(crate) async fn load_side(
    module_map_rc: Rc<RefCell<ModuleMap>>,
    specifier: &str,
  ) -> Result<ModuleId, Error> {
    let id = ModuleMap::next_module_id(module_map_rc.clone());
    let mut module_map = (*module_map_rc).borrow_mut();
    let info = ModuleInfo {
      id,
      main: false,
      name: specifier.to_string(),
    };
    module_map.by_id.insert(id, info);
    Ok(id)
  }
}
