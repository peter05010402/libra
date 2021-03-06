// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    cluster::Cluster,
    effects::{Effect, StopContainer},
    experiments::{Context, Experiment, ExperimentParam},
    instance,
    instance::Instance,
    stats,
    tx_emitter::EmitJobRequest,
    util::unix_timestamp_now,
};
use async_trait::async_trait;
use debug_interface::node_debug_service::parse_event;
use futures::{future::join_all, join};
use libra_logger::info;
use serde_json::Value;
use std::{
    collections::HashSet,
    fmt::{Display, Error, Formatter},
    sync::atomic::Ordering,
    time::Duration,
};
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
pub struct PerformanceBenchmarkParams {
    #[structopt(
        long,
        default_value = "0",
        help = "Percent of nodes which should be down"
    )]
    pub percent_nodes_down: usize,
    #[structopt(
        long,
        help = "Whether cluster test should run against validators or full nodes"
    )]
    pub is_fullnode: bool,
    #[structopt(long, help = "Whether benchmark should perform trace")]
    pub trace: bool,
    #[structopt(
    long,
    default_value = Box::leak(format!("{}", DEFAULT_BENCH_DURATION).into_boxed_str()),
    help = "Duration of an experiment in seconds"
    )]
    pub duration: u64,
}

pub struct PerformanceBenchmark {
    down_instances: Vec<Instance>,
    up_instances: Vec<Instance>,
    percent_nodes_down: usize,
    duration: Duration,
    trace: bool,
}

pub const DEFAULT_BENCH_DURATION: u64 = 120;

impl PerformanceBenchmarkParams {
    pub fn new_nodes_down(percent_nodes_down: usize) -> Self {
        Self {
            percent_nodes_down,
            is_fullnode: false,
            duration: DEFAULT_BENCH_DURATION,
            trace: false,
        }
    }
}

impl ExperimentParam for PerformanceBenchmarkParams {
    type E = PerformanceBenchmark;
    fn build(self, cluster: &Cluster) -> Self::E {
        if self.is_fullnode {
            let num_nodes = cluster.fullnode_instances().len();
            let nodes_down = (num_nodes * self.percent_nodes_down) / 100;
            let (down_instances, up_instances) = cluster.split_n_fullnodes_random(nodes_down);
            Self::E {
                down_instances: down_instances.into_fullnode_instances(),
                up_instances: up_instances.into_fullnode_instances(),
                percent_nodes_down: self.percent_nodes_down,
                duration: Duration::from_secs(self.duration),
                trace: self.trace,
            }
        } else {
            let num_nodes = cluster.validator_instances().len();
            let nodes_down = (num_nodes * self.percent_nodes_down) / 100;
            let (down_instances, up_instances) = cluster.split_n_validators_random(nodes_down);
            Self::E {
                down_instances: down_instances.into_validator_instances(),
                up_instances: up_instances.into_validator_instances(),
                percent_nodes_down: self.percent_nodes_down,
                duration: Duration::from_secs(self.duration),
                trace: self.trace,
            }
        }
    }
}

#[async_trait]
impl Experiment for PerformanceBenchmark {
    fn affected_validators(&self) -> HashSet<String> {
        instance::instancelist_to_set(&self.down_instances)
    }

    async fn run(&mut self, context: &mut Context<'_>) -> anyhow::Result<()> {
        let stop_effects: Vec<_> = self
            .down_instances
            .clone()
            .into_iter()
            .map(StopContainer::new)
            .collect();
        let futures = stop_effects.iter().map(|e| e.activate());
        join_all(futures).await;
        let buffer = Duration::from_secs(60);
        let window = self.duration + buffer * 2;
        let emit_job_request = EmitJobRequest::for_instances(
            self.up_instances.clone(),
            context.global_emit_job_request,
        );
        let emit_txn = context.tx_emitter.emit_txn_for(window, emit_job_request);
        let trace_tail = &context.trace_tail;
        let trace_delay = buffer;
        let trace = self.trace;
        let capture_trace = async move {
            if trace {
                tokio::time::delay_for(trace_delay).await;
                Some(trace_tail.capture_trace(Duration::from_secs(5)).await)
            } else {
                None
            }
        };
        let (stats, trace) = join!(emit_txn, capture_trace);
        let stats = stats?;
        if let Some(trace) = trace {
            info!("Traced {} events", trace.len());
            let mut events = vec![];
            for (node, event) in trace {
                let mut event = parse_event(event);
                // This could be done more elegantly, but for now this will do
                event
                    .json
                    .as_object_mut()
                    .unwrap()
                    .insert("peer".to_string(), Value::String(node));
                events.push(event);
            }
            events.sort_by_key(|k| k.timestamp);
            let node =
                debug_interface::libra_trace::random_node(&events[..], "json-rpc::submit", "txn::")
                    .expect("No trace node found");
            info!("Tracing {}", node);
            debug_interface::libra_trace::trace_node(&events[..], &node);
        }
        let end = unix_timestamp_now() - buffer;
        let start = end - window + 2 * buffer;
        let (avg_tps, avg_latency) = stats::txn_stats(&context.prometheus, start, end)?;
        let avg_txns_per_block = stats::avg_txns_per_block(&context.prometheus, start, end)?;
        info!(
            "Link to dashboard : {}",
            context.prometheus.link_to_dashboard(start, end)
        );
        let futures = stop_effects.iter().map(|e| e.deactivate());
        join_all(futures).await;
        let submitted_txn = stats.submitted.load(Ordering::Relaxed);
        let expired_txn = stats.expired.load(Ordering::Relaxed);
        context
            .report
            .report_metric(&self, "submitted_txn", submitted_txn as f64);
        context
            .report
            .report_metric(&self, "expired_txn", expired_txn as f64);
        context
            .report
            .report_metric(&self, "avg_txns_per_block", avg_txns_per_block as f64);
        context.report.report_metric(&self, "avg_tps", avg_tps);
        context
            .report
            .report_metric(&self, "avg_latency", avg_latency);
        info!("avg_txns_per_block: {}", avg_txns_per_block);
        let expired_text = if expired_txn == 0 {
            "no expired txns".to_string()
        } else {
            format!("(!) expired {} out of {} txns", expired_txn, submitted_txn)
        };
        context.report.report_text(format!(
            "{} : {:.0} TPS, {:.1} ms latency, {}",
            self, avg_tps, avg_latency, expired_text
        ));
        Ok(())
    }

    fn deadline(&self) -> Duration {
        Duration::from_secs(600)
    }
}

impl Display for PerformanceBenchmark {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        if self.percent_nodes_down == 0 {
            write!(f, "all up")
        } else {
            write!(f, "{}% down", self.percent_nodes_down)
        }
    }
}
