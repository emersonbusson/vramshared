// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/operation_banner/package.rs
//! Blue dashboard banner for measured package installation work.

use super::*;

pub(super) fn build_package_banner(
    progress: &OperationProgressV1,
    language: Language,
    now_unix_ms: u64,
) -> OperationBannerContent {
    let (label, files_label) = match language {
        Language::En => ("INSTALLATION", "files"),
        Language::PtBr => ("INSTALAÇÃO", "arquivos"),
    };
    let title = match progress.target.as_deref() {
        Some(release) => format!(" {label} · {release} "),
        None => format!(" {label} "),
    };
    let phase = match (language, progress.phase.as_str()) {
        (Language::En, PACKAGE_VERIFY) => "verifying package",
        (Language::PtBr, PACKAGE_VERIFY) => "verificando pacote",
        (Language::En, PACKAGE_STAGE) => "staging package files",
        (Language::PtBr, PACKAGE_STAGE) => "preparando arquivos do pacote",
        (Language::En, _) => "committing package",
        (Language::PtBr, _) => "efetivando instalação",
    };
    let step = dashboard_phrase(language, DashboardPhrase::StepLabel);
    let position = format!("{step} {}/{}", progress.step_index, progress.total_steps);
    let elapsed = progress.elapsed_secs(now_unix_ms);
    let (primary_line, secondary_line) = match progress.package_files {
        Some(files) => {
            let pct = files.percent();
            let bar = make_bar(u64::from(pct), BANNER_BAR_WIDTH);
            (
                format!(" {bar} {pct:>3}% · {phase} · {position}"),
                format!(
                    " {} / {} {files_label} · {elapsed}s",
                    files.completed, files.total
                ),
            )
        }
        None => (format!(" {phase} · {position}"), format!(" {elapsed}s")),
    };
    OperationBannerContent {
        title,
        primary_line,
        secondary_line,
        border_color: Color::Cyan,
    }
}
