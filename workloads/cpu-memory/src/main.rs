use std::process::ExitCode;

use snapforge_core::workload::WorkloadKind;

fn main() -> ExitCode {
    snapforge_core::runner::run(WorkloadKind::CpuMemory, std::env::args().skip(1))
}
