/*
Portions Copyright 2020-2021 ZomboDB, LLC.
Portions Copyright 2021-2022 Technology Concepts & Design, Inc. <support@tcdi.com>

All rights reserved.

Use of this source code is governed by the PostgreSQL license that can be found in the LICENSE.md file.
*/

use crate::{
    gucs,
    user_crate::{StateLoaded, UserCrate},
};
use pgx::{pg_sys::FunctionCallInfo, *};
use std::{
    cell::RefCell,
    collections::{hash_map::Entry, HashMap},
    env::consts::DLL_SUFFIX,
    path::PathBuf,
    process::Output,
};

thread_local! {
    pub(crate) static LOADED_SYMBOLS: RefCell<HashMap<pg_sys::Oid, UserCrate<StateLoaded>>> = Default::default();
}

pub(crate) fn init() {
    ()
}

#[tracing::instrument(level = "debug")]
pub(crate) unsafe fn unload_function(fn_oid: pg_sys::Oid) {
    LOADED_SYMBOLS.with(|loaded_symbols| {
        let mut loaded_symbols_handle = loaded_symbols.borrow_mut();
        let removed = loaded_symbols_handle.remove(&fn_oid);
        if let Some(user_crate) = removed {
            tracing::info!("unloaded function");
            user_crate.close().unwrap();
        }
    })
}

#[tracing::instrument(level = "debug")]
pub(crate) unsafe fn evaluate_function(
    fn_oid: pg_sys::Oid,
    fcinfo: FunctionCallInfo,
) -> eyre::Result<pg_sys::Datum> {
    LOADED_SYMBOLS.with(|loaded_symbols| {
        let mut loaded_symbols_handle = loaded_symbols.borrow_mut();
        let user_crate_loaded = match loaded_symbols_handle.entry(fn_oid) {
            entry @ Entry::Occupied(_) => {
                entry.or_insert_with(|| unreachable!("Occupied entry was vacant"))
            }
            entry @ Entry::Vacant(_) => {
                let crate_name = crate_name(fn_oid);
                let mut shared_object_name = crate_name;
                #[cfg(any(
                    all(target_os = "macos", target_arch = "x86_64"),
                    feature = "force_enable_x86_64_darwin_generations"
                ))]
                {
                    let (latest, _path) =
                        crate::generation::latest_generation(&shared_object_name, true)
                            .unwrap_or_default();

                    shared_object_name.push_str(&format!("_{}", latest));
                };
                shared_object_name.push_str(DLL_SUFFIX);

                let shared_library = gucs::work_dir().join(&shared_object_name);
                let user_crate_built = UserCrate::built(fn_oid, shared_library);
                let user_crate_loaded = user_crate_built.load()?;

                entry.or_insert(user_crate_loaded)
            }
        };

        tracing::trace!(
            "Evaluating symbol {:?} from {}",
            user_crate_loaded.symbol_name(),
            user_crate_loaded.shared_object().display()
        );

        Ok(user_crate_loaded.evaluate(fcinfo))
    })
}

#[tracing::instrument(level = "debug")]
pub(crate) fn compile_function(fn_oid: pg_sys::Oid) -> eyre::Result<(PathBuf, Output)> {
    let work_dir = gucs::work_dir();
    let pg_config = gucs::pg_config();
    let target_dir = work_dir.join("target");

    let generated = unsafe { UserCrate::try_from_fn_oid(fn_oid)? };
    let provisioned = generated.provision(&work_dir)?;
    let (built, output) = provisioned.build(&work_dir, pg_config, Some(target_dir.as_path()))?;

    let shared_object = built.shared_object();

    Ok((shared_object.into(), output))
}

pub(crate) fn crate_name(fn_oid: pg_sys::Oid) -> String {
    let crate_name = format!("plrust_fn_oid_{}", fn_oid);

    crate_name
}
