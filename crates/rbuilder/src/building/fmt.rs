use std::fmt::Write;

use crate::utils::fmt::write_indent;

use super::ExecutionResult;
use rbuilder_primitives::fmt::{write_order, write_sim_value};

pub fn write_exec_res<Buffer: Write>(
    indent: usize,
    buf: &mut Buffer,
    exec_res: &ExecutionResult,
) -> std::fmt::Result {
    write_indent(indent, buf)?;
    buf.write_str(&format!("ExecResult {}:\n", exec_res.order.id()))?;

    write_indent(indent + 1, buf)?;
    buf.write_str("Sim:\n")?;

    write_sim_value(indent + 2, buf, &exec_res.inplace_sim)?;

    write_indent(indent + 1, buf)?;
    buf.write_str("Order:\n")?;

    write_order(indent + 2, buf, &exec_res.order)
}
