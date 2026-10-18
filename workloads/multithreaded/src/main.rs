use std::process::ExitCode;

use snapforge_core::workload::WorkloadKind;

fn main() -> ExitCode {
    snapforge_core::runner::run(WorkloadKind::Multithreaded, std::env::args().skip(1))
}
