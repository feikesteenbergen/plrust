/*
Portions Copyright 2020-2021 ZomboDB, LLC.
Portions Copyright 2021-2022 Technology Concepts & Design, Inc. <support@tcdi.com>

All rights reserved.

Use of this source code is governed by the PostgreSQL license that can be found in the LICENSE.md file.
*/

#![doc = include_str!("../README.md")]

mod error;
#[cfg(any(
    all(target_os = "macos", target_arch = "x86_64"),
    feature = "force_enable_x86_64_darwin_generations"
))]
mod generation;
mod gucs;
mod logging;
mod plrust;
mod user_crate;

#[cfg(any(test, feature = "pg_test"))]
pub mod tests;

use error::PlRustError;
use pgx::*;

#[cfg(any(test, feature = "pg_test"))]
pub use tests::pg_test;
pg_module_magic!();

#[pg_guard]
fn _PG_init() {
    color_eyre::config::HookBuilder::default()
        .theme(color_eyre::config::Theme::new())
        .into_hooks()
        .1
        .install()
        .unwrap();

    gucs::init();

    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter_layer = EnvFilter::builder()
        .with_default_directive(gucs::tracing_level().into())
        .from_env()
        .expect("Error parsing default log level");

    let error_layer = tracing_error::ErrorLayer::default();

    let format_layer = tracing_subscriber::fmt::Layer::new()
        .with_ansi(false)
        .with_writer(|| logging::PgxNoticeWriter::<true>)
        .without_time()
        .pretty();
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(format_layer)
        .with(error_layer)
        .try_init()
        .expect("Could not initialize tracing registry");

    plrust::init();
}

/// `pgx` doesn't know how to declare a CREATE FUNCTION statement for a function
/// whose only argument is a `pg_sys::FunctionCallInfo`, so we gotta do that ourselves.
#[pg_extern(sql = "
CREATE FUNCTION plrust_call_handler() RETURNS language_handler
    LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
")]
#[tracing::instrument(level = "debug")]
unsafe fn plrust_call_handler(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    unsafe fn plrust_call_handler_inner(
        fcinfo: pg_sys::FunctionCallInfo,
    ) -> eyre::Result<pg_sys::Datum> {
        let fn_oid = fcinfo
            .as_ref()
            .ok_or(PlRustError::NullFunctionCallInfo)?
            .flinfo
            .as_ref()
            .ok_or(PlRustError::NullFmgrInfo)?
            .fn_oid;
        let retval = plrust::evaluate_function(fn_oid, fcinfo)?;
        Ok(retval)
    }

    match plrust_call_handler_inner(fcinfo) {
        Ok(datum) => datum,
        // Panic into the pgx guard.
        Err(err) => panic!("{:?}", err),
    }
}

#[pg_extern]
#[tracing::instrument(level = "debug")]
unsafe fn plrust_validator(fn_oid: pg_sys::Oid, fcinfo: pg_sys::FunctionCallInfo) {
    unsafe fn plrust_validator_inner(
        fn_oid: pg_sys::Oid,
        fcinfo: pg_sys::FunctionCallInfo,
    ) -> eyre::Result<()> {
        let fcinfo = PgBox::from_pg(fcinfo);
        let flinfo = PgBox::from_pg(fcinfo.flinfo);
        if !pg_sys::CheckFunctionValidatorAccess(
            flinfo.fn_oid,
            pg_getarg(fcinfo.as_ptr(), 0).unwrap(),
        ) {
            return Err(PlRustError::CheckFunctionValidatorAccess)?;
        }

        plrust::unload_function(fn_oid);
        // NOTE:  We purposely ignore the `check_function_bodies` GUC for compilation as we need to
        // compile the function when it's created to avoid locking during function execution
        let (_, output) = plrust::compile_function(fn_oid)?;

        // however, we'll use it to decide if we should go ahead and dynamically load our function
        if pg_sys::check_function_bodies {
            // it's on, so lets go ahead and load our function
            // plrust::lookup_function(fn_oid);
        }

        // if the compilation had warnings we'll display them
        let stderr =
            String::from_utf8(output.stdout.clone()).expect("`cargo`'s stdout was not UTF-8");
        if stderr.contains("warning: ") {
            pgx::warning!("\n{}", stderr);
        }

        Ok(())
    }

    match plrust_validator_inner(fn_oid, fcinfo) {
        Ok(()) => (),
        // Panic into the pgx guard.
        Err(err) => panic!("{:?}", err),
    }
}

#[pg_extern]
#[tracing::instrument(level = "debug")]
fn recompile_function(
    fn_oid: pg_sys::Oid,
) -> (
    name!(library_path, Option<String>),
    name!(stdout, Option<String>),
    name!(stderr, Option<String>),
    name!(plrust_error, Option<String>),
) {
    unsafe {
        plrust::unload_function(fn_oid);
    }
    match plrust::compile_function(fn_oid) {
        Ok((work_dir, output)) => (
            Some(work_dir.display().to_string()),
            Some(String::from_utf8(output.stdout.clone()).expect("`cargo`'s stdout was not UTF-8")),
            Some(String::from_utf8(output.stderr.clone()).expect("`cargo`'s stderr was not UTF-8")),
            None,
        ),
        Err(err) => (None, None, None, Some(format!("{:?}", err))),
    }
}

extension_sql!(
    r#"
CREATE LANGUAGE plrust
    HANDLER plrust.plrust_call_handler
    VALIDATOR plrust.plrust_validator;

COMMENT ON LANGUAGE plrust IS 'PL/rust procedural language';
"#,
    name = "language_handler",
    requires = [plrust_call_handler, plrust_validator]
);
