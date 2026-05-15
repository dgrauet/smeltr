//! `smeltr analyze` command.

use anyhow::Context;
use anyhow::Result;
use smeltr_analyzer::analyze;
use smeltr_core::reader::read_events;

pub fn run(arg_last: bool, session_id: Option<String>, include_ambient: bool) -> Result<()> {
    let dir = crate::session_resolver::resolve(session_id, arg_last, include_ambient)?;
    let events =
        read_events(&dir).with_context(|| format!("reading events from {}", dir.display()))?;
    let report = analyze(&events);
    println!("{}", report.render());
    Ok(())
}
