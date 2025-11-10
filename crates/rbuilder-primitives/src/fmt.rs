use std::fmt::Write;

use super::{Order, ShareBundleBody, ShareBundleInner, ShareBundleTx, SimValue, SimulatedOrder};

/// Writes indent spaces
pub fn write_indent<Buffer: Write>(indent: usize, buf: &mut Buffer) -> std::fmt::Result {
    buf.write_str(&format!("{: <1$}", "", indent))
}

pub fn write_share_bundle_tx<Buffer: Write>(
    indent: usize,
    buf: &mut Buffer,
    tx: &ShareBundleTx,
) -> std::fmt::Result {
    write_indent(indent, buf)?;
    buf.write_str(&format!(
        "TX {} Rev  {:?} val {}\n",
        tx.tx.hash(),
        tx.revert_behavior,
        tx.tx.value()
    ))
}

pub fn write_share_bundle_inner<Buffer: Write>(
    indent: usize,
    buf: &mut Buffer,
    inner: &ShareBundleInner,
) -> std::fmt::Result {
    write_indent(indent, buf)?;
    buf.write_str(&format!("Inner can skip {} \n", inner.can_skip))?;
    for item in &inner.body {
        match item {
            ShareBundleBody::Tx(tx) => write_share_bundle_tx(indent + 1, buf, tx)?,
            ShareBundleBody::Bundle(sb) => write_share_bundle_inner(indent + 1, buf, sb)?,
        }
    }
    Ok(())
}

pub fn write_order<Buffer: Write>(
    indent: usize,
    buf: &mut Buffer,
    order: &Order,
) -> std::fmt::Result {
    write_indent(indent, buf)?;
    match order {
        Order::Bundle(b) => buf.write_str(&format!("B {}\n", b.hash)),
        Order::Tx(tx) => buf.write_str(&format!(
            "Tx {} val {}\n",
            tx.tx_with_blobs.hash(),
            tx.tx_with_blobs.value()
        )),
        Order::ShareBundle(sb) => {
            buf.write_str(&format!("ShB {:?}\n", sb.hash))?;
            write_share_bundle_inner(indent + 1, buf, &sb.inner_bundle)
        }
    }
}

pub fn write_sim_value<Buffer: Write>(
    indent: usize,
    buf: &mut Buffer,
    sim_value: &SimValue,
) -> std::fmt::Result {
    write_indent(indent, buf)?;
    std::fmt::write(
        buf,
        format_args!(
            "full coinbase_profit {} full mev_gas_price {}",
            sim_value.full_profit_info().coinbase_profit(),
            sim_value.full_profit_info().mev_gas_price()
        ),
    )?;
    std::fmt::write(
        buf,
        format_args!(
            "non mempool coinbase_profit {} non mempool mev_gas_price {}",
            sim_value.non_mempool_profit_info().coinbase_profit(),
            sim_value.non_mempool_profit_info().mev_gas_price()
        ),
    )?;
    buf.write_str(" Kickbacks ")?;
    for kb in sim_value.paid_kickbacks() {
        buf.write_str(&format!("{}->{},", kb.0, kb.1))?;
    }
    buf.write_str("\n")
}

pub fn write_sim_order<Buffer: Write>(
    indent: usize,
    buf: &mut Buffer,
    sim_order: &SimulatedOrder,
) -> std::fmt::Result {
    write_indent(indent, buf)?;
    std::fmt::write(buf, format_args!("SimulatedOrder {}", sim_order.id()))?;

    write_indent(indent + 1, buf)?;
    buf.write_str("Sim")?;
    write_sim_value(indent + 2, buf, &sim_order.sim_value)?;

    write_indent(indent + 1, buf)?;
    buf.write_str("Order")?;
    write_order(indent + 2, buf, &sim_order.order)
}
