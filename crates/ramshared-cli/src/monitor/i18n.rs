// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/i18n.rs
//! Small, dependency-free text catalog for the terminal dashboard.
//!
//! The wire document, logs and JSON keep their English identifiers. Only the
//! display face is translated here; protocol tokens such as `ACTIVE` are
//! mapped by [`status_label`], never rewritten in the document.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    En,
    PtBr,
}

impl Language {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "en" | "EN" | "English" => Some(Self::En),
            "pt-br" | "pt_BR" | "pt_BR.UTF-8" | "pt-BR" | "Portuguese" => Some(Self::PtBr),
            _ => None,
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::En => Self::PtBr,
            Self::PtBr => Self::En,
        }
    }

    #[allow(dead_code)]
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::PtBr => "pt-BR",
        }
    }

    pub fn number(self, value: String) -> String {
        match self {
            Self::En => value,
            Self::PtBr => value.replace('.', ","),
        }
    }
}

#[allow(dead_code)]
pub struct Text {
    pub host_up: &'static str,
    pub overall: &'static str,
    pub swap: &'static str,
    pub cache_gpu: &'static str,
    pub origin: &'static str,
    pub pressure: &'static str,
    pub pressure_guest: &'static str,
    pub memory: &'static str,
    pub guest_wsl2: &'static str,
    pub ram: &'static str,
    pub zram: &'static str,
    pub ssd: &'static str,
    pub file: &'static str,
    pub compression: &'static str,
    pub cpu: &'static str,
    pub cost: &'static str,
    pub incompressible: &'static str,
    pub gpu: &'static str,
    pub total: &'static str,
    pub usable: &'static str,
    pub used: &'static str,
    pub free: &'static str,
    pub gpu_busy: &'static str,
    pub cache_origin: &'static str,
    pub active: &'static str,
    pub target: &'static str,
    pub occupancy: &'static str,
    pub effectiveness: &'static str,
    pub last_5_min: &'static str,
    pub now: &'static str,
    pub hits: &'static str,
    pub origin_reads: &'static str,
    pub served: &'static str,
    pub origin_reads_rate: &'static str,
    pub physical: &'static str,
    pub logical: &'static str,
    pub codec: &'static str,
    pub no_codec: &'static str,
    pub ratio_unavailable: &'static str,
    pub state: &'static str,
    pub off: &'static str,
    pub lease_lost: &'static str,
    pub cache_active: &'static str,
    pub capacity: &'static str,
    pub logical_capacity: &'static str,
    pub free_capacity: &'static str,
    pub ago: &'static str,
    pub action: &'static str,
    pub nothing_critical: &'static str,
    pub control_reserve: &'static str,
    pub details: &'static str,
    pub glossary: &'static str,
    pub quit: &'static str,
    pub back: &'static str,
    pub language: &'static str,
    pub switch_language: &'static str,
    pub not_measured: &'static str,
    pub unavailable: &'static str,
    pub stale: &'static str,
    pub pressure_page: &'static str,
    pub host_windows: &'static str,
    pub guest_psi: &'static str,
    pub thresholds: &'static str,
    pub supervisor: &'static str,
    /// The control-plane authority's own state, beside the cache's state.
    pub control_plane: &'static str,
    /// The lease deadline, as the operator reads it.
    pub lease_deadline: &'static str,
    pub why_degraded: &'static str,
    pub recovery: &'static str,
    pub reserve: &'static str,
    pub raw_only: &'static str,
    pub no_spill: &'static str,
    pub counters: &'static str,
    pub glossary_text: &'static [&'static str],
    pub pressure_help: &'static [&'static str],
}

pub const EN: Text = Text {
    host_up: "host up",
    overall: "Overall",
    swap: "Swap",
    cache_gpu: "GPU cache",
    origin: "Origin",
    pressure: "Pressure",
    pressure_guest: "Guest pressure",
    memory: "MEMORY",
    guest_wsl2: "guest WSL2",
    ram: "RAM",
    zram: "zram",
    ssd: "SSD",
    file: "file",
    compression: "COMPRESSION",
    cpu: "CPU",
    cost: "cost",
    incompressible: "incompressible",
    gpu: "GPU",
    total: "Total",
    usable: "Usable",
    used: "Used",
    free: "Free",
    gpu_busy: "GPU busy: not measured",
    cache_origin: "ORIGIN CACHE (VRAM)",
    active: "ACTIVE",
    target: "target",
    occupancy: "OCCUPANCY",
    effectiveness: "EFFECTIVENESS",
    last_5_min: "last 5 min",
    now: "now",
    hits: "hits",
    origin_reads: "origin reads",
    served: "served",
    origin_reads_rate: "origin reads/s",
    physical: "physical",
    logical: "logical",
    codec: "CODEC",
    no_codec: "no codec",
    ratio_unavailable: "ratio unavailable",
    state: "STATE",
    off: "OFF",
    lease_lost: "lease lost",
    cache_active: "cache ACTIVE",
    capacity: "CAPACITY",
    logical_capacity: "logical",
    free_capacity: "free",
    ago: "ago",
    action: "ACTION",
    nothing_critical: "Nothing critical now",
    control_reserve: "control reserve",
    details: "Details",
    glossary: "Glossary",
    quit: "Quit",
    back: "back",
    language: "Language",
    switch_language: "Português (Brasil)",
    not_measured: "not measured",
    unavailable: "unavailable",
    stale: "stale",
    pressure_page: "PRESSURE",
    host_windows: "HOST WINDOWS",
    guest_psi: "GUEST WSL2 (PSI)",
    thresholds: "limits: <1% healthy · >=2% guard · >=5% critical · >=10% emergency",
    supervisor: "supervisor",
    control_plane: "CONTROL PLANE",
    lease_deadline: "lease deadline",
    why_degraded: "why degraded",
    recovery: "recovery",
    reserve: "reserve",
    raw_only: "RawOnly",
    no_spill: "the cache is not a swap tier and has no spill order",
    counters: "COUNTERS",
    glossary_text: &[
        "zram: compressed RAM swap on CPU (lzo-rle); Tier 1 before NBD/SSD.",
        "1:1 pages: 4 KiB pages already encrypted/compressed that zram stores uncompressed (4 KiB each).",
        "cache (VRAM): GPU CUDA PTX LZ4 compressed read-through cache over the authoritative SSD origin.",
        "hit: read served from VRAM; origin read: cache miss served by SSD; GPU writes: batches pushed to VRAM.",
        "RawOnly: cache stores raw bytes 1:1; no GPU codec is active.",
        "PSI: percentage of time tasks stall on memory, not % RAM used.",
        "control reserve: memory the control plane keeps free to avoid host freezes.",
    ],
    pressure_help: &[
        "full: all non-idle tasks stalled; the supervisor trigger.",
        "some: at least one task stalled; context only, never a trigger.",
        "thresholds are policy, not a 0-100% limit.",
    ],
};

pub const PT: Text = Text {
    host_up: "host ativo há",
    overall: "Geral",
    swap: "Swap",
    cache_gpu: "Cache GPU",
    origin: "Origem",
    pressure: "Pressão",
    pressure_guest: "Pressão do guest",
    memory: "MEMÓRIA",
    guest_wsl2: "guest WSL2",
    ram: "RAM",
    zram: "zram",
    ssd: "SSD",
    file: "arquivo",
    compression: "COMPRESSÃO",
    cpu: "CPU",
    cost: "custo",
    incompressible: "incompressíveis",
    gpu: "GPU",
    total: "Total",
    usable: "Usável",
    used: "Usado",
    free: "Livre",
    gpu_busy: "GPU ocupada: não medida",
    cache_origin: "CACHE DA ORIGEM (VRAM)",
    active: "ATIVO",
    target: "alvo",
    occupancy: "OCUPAÇÃO",
    effectiveness: "EFETIVIDADE",
    last_5_min: "últimos 5 min",
    now: "agora",
    hits: "acertos",
    origin_reads: "na origem",
    served: "servido",
    origin_reads_rate: "na origem/s",
    physical: "físico",
    logical: "lógico",
    codec: "CODEC",
    no_codec: "sem codec",
    ratio_unavailable: "razão indisponível",
    state: "ESTADO",
    off: "DESLIGADO",
    lease_lost: "autorização perdida",
    cache_active: "cache ATIVO",
    capacity: "CAPACIDADE",
    logical_capacity: "lógico",
    free_capacity: "livre",
    ago: "atrás",
    action: "AÇÃO",
    nothing_critical: "Nada crítico agora",
    control_reserve: "reserva do controle",
    details: "Detalhes",
    glossary: "Glossário",
    quit: "Sair",
    back: "voltar",
    language: "Idioma",
    switch_language: "English",
    not_measured: "não medido",
    unavailable: "indisponível",
    stale: "desatualizado",
    pressure_page: "PRESSÃO",
    host_windows: "WINDOWS (host)",
    guest_psi: "GUEST WSL2 (PSI)",
    thresholds: "limites: <1% saudável · ≥2% guarda · ≥5% crítico · ≥10% emergência",
    supervisor: "supervisor",
    control_plane: "PLANO DE CONTROLE",
    lease_deadline: "prazo do lease",
    why_degraded: "por que degradado",
    recovery: "recuperação",
    reserve: "reserva",
    raw_only: "RawOnly",
    no_spill: "o cache não é tier de swap e não tem ordem de spill",
    counters: "CONTADORES",
    glossary_text: &[
        "zram: swap comprimido na RAM pela CPU (lzo-rle); é o Tier 1 antes de NBD/SSD.",
        "págs 1:1: páginas de 4 KiB já comprimidas/aleatórias que não encolhem e ficam 1:1 (4 KiB cada) no zram.",
        "cache (VRAM): cache comprimido na GPU (CUDA PTX LZ4) sobre a origem SSD autoritativa.",
        "acerto: leitura servida da VRAM; na origem: cache miss lido do SSD; escritas GPU: lotes gravados na VRAM.",
        "RawOnly: o cache guarda bytes crus 1:1; nenhum codec GPU está ativo.",
        "PSI: % do tempo em que tarefas travam por memória, não % de RAM usada.",
        "reserva do controle: memória que o plano de controle mantém livre para evitar travamentos.",
    ],
    pressure_help: &[
        "full: todas as tarefas não-idle travadas; é o gatilho do supervisor.",
        "some: pelo menos uma tarefa travada; só contexto, nunca gatilho.",
        "os limites são política, não um teto de 0–100%.",
    ],
};

pub fn text(language: Language) -> &'static Text {
    match language {
        Language::En => &EN,
        Language::PtBr => &PT,
    }
}

/// The catalog's labels are the operator's reading of the product's own
/// states, so every mapping is exercised in both languages: a state that
/// falls through to the raw string is a state the operator reads in English
/// while the rest of the screen is in Portuguese.
#[cfg(test)]
mod label_tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn every_status_label_is_localized_in_both_languages() {
        for raw in [
            "READY",
            "ACTIVE",
            "DEGRADED",
            "AT_RISK",
            "AT RISK",
            "HEALTHY",
            "OFF",
            "BLOCKED",
            "UNKNOWN",
            "STALE",
            "SAFE_MODE",
            "SAFE MODE",
            "CRITICAL",
            "EMERGENCY",
            "GUARDED",
        ] {
            let english = status_label(Language::En, raw);
            let portuguese = status_label(Language::PtBr, raw);
            assert!(!english.is_empty(), "{raw}");
            assert!(!portuguese.is_empty(), "{raw}");
        }
        // An unknown state keeps its own spelling: the screen must never
        // invent a state the product did not publish.
        assert_eq!(
            status_label(Language::PtBr, "SOMETHING_NEW"),
            "SOMETHING_NEW"
        );
    }

    #[test]
    fn the_cache_reasons_are_read_in_the_operator_language() {
        assert_eq!(
            reason_label(Language::PtBr, "lease denied: lease expired"),
            "autorização expirada"
        );
        assert_eq!(
            reason_label(Language::En, "lease denied: lease expired"),
            "lease expired"
        );
        assert_eq!(
            reason_label(Language::PtBr, "safe mode after disconnect"),
            "modo seguro após desconexão"
        );
        assert_eq!(
            reason_label(
                Language::PtBr,
                "cache admission requires a live vsock lease"
            ),
            "sem autorização do host"
        );
        assert_eq!(
            reason_label(Language::PtBr, "no lease"),
            "sem autorização do host"
        );
        // An unknown reason keeps its own text.
        assert_eq!(
            reason_label(Language::PtBr, "the worker exited"),
            "the worker exited"
        );
    }

    #[test]
    fn the_pressure_class_thresholds_are_the_published_ones() {
        assert_eq!(pressure_class(0.5), "LOW");
        assert_eq!(pressure_class(1.0), "ATTENTION");
        assert_eq!(pressure_class(2.0), "GUARDED");
        assert_eq!(pressure_class(5.0), "CRITICAL");
        assert_eq!(pressure_class(10.0), "EMERGENCY");
        assert_eq!(pressure_label(Language::PtBr, "GUARDED"), "GUARDA");
        assert_eq!(pressure_label(Language::En, "GUARDED"), "GUARD");
        assert_eq!(pressure_label(Language::En, "UNKNOWN-CLASS"), "UNKNOWN");
    }
}

pub fn status_label(language: Language, raw: &str) -> String {
    let value = match raw {
        "READY" => match language {
            Language::En => "READY",
            Language::PtBr => "PRONTO",
        },
        "ACTIVE" => match language {
            Language::En => "ACTIVE",
            Language::PtBr => "ATIVO",
        },
        "DEGRADED" => match language {
            Language::En => "DEGRADED",
            Language::PtBr => "DEGRADADO",
        },
        "AT_RISK" | "AT RISK" => match language {
            Language::En => "AT RISK",
            Language::PtBr => "EM RISCO",
        },
        "HEALTHY" => match language {
            Language::En => "HEALTHY",
            Language::PtBr => "SAUDÁVEL",
        },
        "OFF" => match language {
            Language::En => "OFF",
            Language::PtBr => "DESLIGADO",
        },
        "BLOCKED" => match language {
            Language::En => "BLOCKED",
            Language::PtBr => "BLOQUEADO",
        },
        "UNKNOWN" => match language {
            Language::En => "UNKNOWN",
            Language::PtBr => "DESCONHECIDO",
        },
        "STALE" => match language {
            Language::En => "STALE",
            Language::PtBr => "DESATUALIZADO",
        },
        "SAFE_MODE" | "SAFE MODE" => match language {
            Language::En => "SAFE MODE",
            Language::PtBr => "MODO SEGURO",
        },
        "CRITICAL" => match language {
            Language::En => "CRITICAL",
            Language::PtBr => "CRÍTICO",
        },
        "EMERGENCY" => match language {
            Language::En => "EMERGENCY",
            Language::PtBr => "EMERGÊNCIA",
        },
        "GUARDED" => match language {
            Language::En => "GUARDED",
            Language::PtBr => "EM GUARDA",
        },
        other => other,
    };
    value.to_string()
}

pub fn reason_label(language: Language, raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let value = if lower.contains("lease expired") {
        match language {
            Language::En => "lease expired",
            Language::PtBr => "autorização expirada",
        }
    } else if lower.contains("safe mode after disconnect") {
        match language {
            Language::En => "safe mode after disconnect",
            Language::PtBr => "modo seguro após desconexão",
        }
    } else if lower.contains("requires a live vsock lease") || lower.contains("no lease") {
        match language {
            Language::En => "no host lease",
            Language::PtBr => "sem autorização do host",
        }
    } else {
        raw
    };
    value.to_string()
}

pub fn pressure_class(full_avg10: f64) -> &'static str {
    if full_avg10 >= 10.0 {
        "EMERGENCY"
    } else if full_avg10 >= 5.0 {
        "CRITICAL"
    } else if full_avg10 >= 2.0 {
        "GUARDED"
    } else if full_avg10 >= 1.0 {
        "ATTENTION"
    } else {
        "LOW"
    }
}

pub fn pressure_label(language: Language, class: &str) -> &'static str {
    match (language, class) {
        (Language::En, "LOW") => "LOW",
        (Language::En, "ATTENTION") => "ATTENTION",
        (Language::En, "GUARDED") => "GUARD",
        (Language::En, "CRITICAL") => "CRITICAL",
        (Language::En, "EMERGENCY") => "EMERGENCY",
        (Language::PtBr, "LOW") => "BAIXA",
        (Language::PtBr, "ATTENTION") => "ATENÇÃO",
        (Language::PtBr, "GUARDED") => "GUARDA",
        (Language::PtBr, "CRITICAL") => "CRÍTICA",
        (Language::PtBr, "EMERGENCY") => "EMERGÊNCIA",
        (_, _) => "UNKNOWN",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DashboardPhrase {
    CheckCacheOrigin,
    ZeroToPeak,
    CumulativeSamples,
    CacheLegend,
    CacheReadThroughNote,
    EffectiveRatio,
    Workspace,
    RecentSamplesPct,
    HostTelemetryAbsent,
    DegradedVerdictNote,
    HostMemoryVsPsiNote,
    CountersSummary,
    RestartPlanHint,
    RealTimeSpeed,
    PageFaultsRate,
    SampleAge,
    OperationBannerTitle,
    DemoteInProgress,
    CacheStarting,
    StepLabel,
    RemainingLabel,
    ReadLabel,
    WriteLabel,
    BatchWrites,
    GpuCodecName,
    SlabLabel,
    SavedLabel,
    PagesOneToOne,
    CodecAwaitingTier2,
    Tier2SwapLabel,
    CompressedBytesLabel,
}

pub(super) fn dashboard_phrase(language: Language, phrase: DashboardPhrase) -> &'static str {
    match (language, phrase) {
        (Language::En, DashboardPhrase::CheckCacheOrigin) => "Check cache and origin status above.",
        (Language::PtBr, DashboardPhrase::CheckCacheOrigin) => {
            "Confira os estados do cache e da origem acima."
        }
        (Language::En, DashboardPhrase::ZeroToPeak) => "0–peak",
        (Language::PtBr, DashboardPhrase::ZeroToPeak) => "0–pico",
        (Language::En, DashboardPhrase::CumulativeSamples) => {
            " Cumulative samples · newest at right"
        }
        (Language::PtBr, DashboardPhrase::CumulativeSamples) => {
            " Amostras acumuladas · mais recente à direita"
        }
        (Language::En, DashboardPhrase::CacheLegend) => {
            "Legend: hit = served by VRAM; origin = cache miss; block = active."
        }
        (Language::PtBr, DashboardPhrase::CacheLegend) => {
            "Legenda: acerto = servido pela VRAM; na origem = cache miss; bloco = ativo."
        }
        (Language::En, DashboardPhrase::CacheReadThroughNote) => {
            "The cache is a read-through copy of the origin; it is not swap."
        }
        (Language::PtBr, DashboardPhrase::CacheReadThroughNote) => {
            "O cache é uma cópia de leitura da origem; não é swap."
        }
        (Language::En, DashboardPhrase::EffectiveRatio) => "effective ratio",
        (Language::PtBr, DashboardPhrase::EffectiveRatio) => "razão efetiva",
        (Language::En, DashboardPhrase::Workspace) => "workspace",
        (Language::PtBr, DashboardPhrase::Workspace) => "workspace",
        (Language::En, DashboardPhrase::RecentSamplesPct) => "recent samples, 0–100%",
        (Language::PtBr, DashboardPhrase::RecentSamplesPct) => "amostras recentes, 0–100%",
        (Language::En, DashboardPhrase::HostTelemetryAbsent) => {
            "Host memory telemetry was not published by this host; no host PSI is invented."
        }
        (Language::PtBr, DashboardPhrase::HostTelemetryAbsent) => {
            "A telemetria de memória do host não foi publicada; nenhum PSI do host é inventado."
        }
        (Language::En, DashboardPhrase::DegradedVerdictNote) => {
            "DEGRADED is the control plane verdict, not the pressure row."
        }
        (Language::PtBr, DashboardPhrase::DegradedVerdictNote) => {
            "DEGRADADO é o veredito do plano de controle, não a linha de pressão."
        }
        (Language::En, DashboardPhrase::HostMemoryVsPsiNote) => {
            "Host memory is not PSI; the guest PSI above is separate."
        }
        (Language::PtBr, DashboardPhrase::HostMemoryVsPsiNote) => {
            "Memória do host não é PSI; o PSI do guest acima é separado."
        }
        (Language::En, DashboardPhrase::CountersSummary) => {
            "hits, origin reads, served bytes, codec errors/timeouts."
        }
        (Language::PtBr, DashboardPhrase::CountersSummary) => {
            "acertos, leituras na origem, bytes servidos, erros/timeouts do codec."
        }
        (Language::En, DashboardPhrase::RestartPlanHint) => {
            "Restart: sudo ramshared down --plan (read-only headroom check)"
        }
        (Language::PtBr, DashboardPhrase::RestartPlanHint) => {
            "Reiniciar: sudo ramshared down --plan (checagem read-only de headroom)"
        }
        (Language::En, DashboardPhrase::RealTimeSpeed) => "Real-Time Speed",
        (Language::PtBr, DashboardPhrase::RealTimeSpeed) => "Velocidade atual",
        (Language::En, DashboardPhrase::PageFaultsRate) => "Page Faults Rate",
        (Language::PtBr, DashboardPhrase::PageFaultsRate) => "Taxa de page faults",
        (Language::En, DashboardPhrase::SampleAge) => "Telemetry Sample Age",
        (Language::PtBr, DashboardPhrase::SampleAge) => "Idade da amostra",
        (Language::En, DashboardPhrase::OperationBannerTitle) => "ACTIVE OPERATION",
        (Language::PtBr, DashboardPhrase::OperationBannerTitle) => "OPERAÇÃO EM ANDAMENTO",
        (Language::En, DashboardPhrase::DemoteInProgress) => "Draining VRAM cache to SSD",
        (Language::PtBr, DashboardPhrase::DemoteInProgress) => "Drenando cache VRAM para SSD",
        (Language::En, DashboardPhrase::CacheStarting) => "Initializing GPU cache worker",
        (Language::PtBr, DashboardPhrase::CacheStarting) => "Inicializando worker do cache GPU",
        (Language::En, DashboardPhrase::StepLabel) => "step",
        (Language::PtBr, DashboardPhrase::StepLabel) => "etapa",
        (Language::En, DashboardPhrase::RemainingLabel) => "remaining",
        (Language::PtBr, DashboardPhrase::RemainingLabel) => "restante",
        (Language::En, DashboardPhrase::ReadLabel) => "Read",
        (Language::PtBr, DashboardPhrase::ReadLabel) => "Leitura",
        (Language::En, DashboardPhrase::WriteLabel) => "Write",
        (Language::PtBr, DashboardPhrase::WriteLabel) => "Escrita",
        (Language::En, DashboardPhrase::BatchWrites) => "GPU writes",
        (Language::PtBr, DashboardPhrase::BatchWrites) => "escritas GPU",
        (Language::En, DashboardPhrase::GpuCodecName) => "GPU CUDA/PTX LZ4",
        (Language::PtBr, DashboardPhrase::GpuCodecName) => "GPU CUDA/PTX LZ4",
        (Language::En, DashboardPhrase::SlabLabel) => "slab",
        (Language::PtBr, DashboardPhrase::SlabLabel) => "slab",
        (Language::En, DashboardPhrase::SavedLabel) => "saved",
        (Language::PtBr, DashboardPhrase::SavedLabel) => "economizados",
        (Language::En, DashboardPhrase::PagesOneToOne) => "pages 1:1",
        (Language::PtBr, DashboardPhrase::PagesOneToOne) => "págs 1:1 (s/ compr.)",
        (Language::En, DashboardPhrase::CodecAwaitingTier2) => "ready · awaiting Tier 2 writes",
        (Language::PtBr, DashboardPhrase::CodecAwaitingTier2) => {
            "pronto · aguardando uso do Tier 2"
        }
        (Language::En, DashboardPhrase::Tier2SwapLabel) => "NBD",
        (Language::PtBr, DashboardPhrase::Tier2SwapLabel) => "NBD",
        (Language::En, DashboardPhrase::CompressedBytesLabel) => "compressed payload",
        (Language::PtBr, DashboardPhrase::CompressedBytesLabel) => "payload comprimido",
    }
}

pub(super) fn operation_kind_label(
    language: Language,
    kind: crate::operation_progress::OperationKind,
) -> &'static str {
    use crate::operation_progress::OperationKind;
    match (language, kind) {
        (Language::En, OperationKind::Teardown) => "Cascade teardown",
        (Language::PtBr, OperationKind::Teardown) => "Desmontagem da cascata",
        (Language::En, OperationKind::SwapDrain) => "Swap drain",
        (Language::PtBr, OperationKind::SwapDrain) => "Drenagem de swap",
        (Language::En, OperationKind::SwapRebalance) => "Fallback swap rebalance",
        (Language::PtBr, OperationKind::SwapRebalance) => "Rebalanceamento de swap fallback",
        (Language::En, OperationKind::CascadeUp) => "Cascade startup",
        (Language::PtBr, OperationKind::CascadeUp) => "Inicialização da cascata",
        (Language::En, OperationKind::SetupInstall) => "Storage / release setup",
        (Language::PtBr, OperationKind::SetupInstall) => "Configuração / instalação",
        (Language::En, OperationKind::ReleaseSync) => "Release synchronization",
        (Language::PtBr, OperationKind::ReleaseSync) => "Sincronização de release",
        (Language::En, OperationKind::StorageBenchmark) => "Storage speed diagnostic",
        (Language::PtBr, OperationKind::StorageBenchmark) => "Diagnóstico de velocidade de discos",
        (Language::En, OperationKind::StressTest) => "Multi-tier GPU cache stress",
        (Language::PtBr, OperationKind::StressTest) => "Estresse multi-tier e cache GPU",
    }
}

pub(super) fn operation_phase_label(language: Language, phase: &str) -> &str {
    match (language, phase) {
        (Language::En, "prepare") => "preparing",
        (Language::PtBr, "prepare") => "preparando",
        (Language::En, "swapoff") => "draining swap",
        (Language::PtBr, "swapoff") => "drenando swap",
        (Language::En, "reset_zram") => "resetting zram",
        (Language::PtBr, "reset_zram") => "resetando zram",
        (Language::En, "disconnect_nbd") => "detaching origin tier",
        (Language::PtBr, "disconnect_nbd") => "desconectando tier de origem",
        (Language::En, "stop_daemon") => "stopping daemon",
        (Language::PtBr, "stop_daemon") => "encerrando daemon",
        (Language::En, "origin_check") => "verifying SSD origin",
        (Language::PtBr, "origin_check") => "verificando origem SSD",
        (Language::En, "zram_setup") => "arming zram tier",
        (Language::PtBr, "zram_setup") => "ativando tier zram",
        (Language::En, "daemon_start") => "starting daemon & GPU worker",
        (Language::PtBr, "daemon_start") => "iniciando daemon e worker GPU",
        (Language::En, "nbd_attach") => "attaching origin block device",
        (Language::PtBr, "nbd_attach") => "conectando dispositivo de origem",
        (Language::En, "zram_bind") => "binding zram writeback",
        (Language::PtBr, "zram_bind") => "vinculando writeback do zram",
        (Language::En, "prepare_storage") => "preparing storage backend",
        (Language::PtBr, "prepare_storage") => "preparando armazenamento",
        (Language::En, "commit_config") => "committing configuration",
        (Language::PtBr, "commit_config") => "gravando configuração",
        (Language::En, "verify") => "verifying state",
        (Language::PtBr, "verify") => "verificando estado",
        (Language::En, "observe_fleet") => "inspecting active units",
        (Language::PtBr, "observe_fleet") => "inspecionando unidades ativas",
        (Language::En, "restart_services") => "restarting managed services",
        (Language::PtBr, "restart_services") => "reiniciando serviços gerenciados",
        (Language::En, "benchmark_volume") => "measuring volume speed",
        (Language::PtBr, "benchmark_volume") => "medindo velocidade do disco",
        (Language::En, "stress_ramp") => "allocating & spilling across tiers",
        (Language::PtBr, "stress_ramp") => "alocando e distribuindo entre os tiers",
        (Language::En, "stress_verify") => "verifying readback & GPU cache hits",
        (Language::PtBr, "stress_verify") => "validando leitura bit-exata e cache GPU",
        (_, other) => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_non_empty_main_labels() {
        for language in [Language::En, Language::PtBr] {
            let text = text(language);
            assert!(!text.overall.is_empty());
            assert!(!text.cache_origin.is_empty());
            assert!(!text.glossary_text.is_empty());
            assert!(!text.pressure_help.is_empty());
        }
    }

    #[test]
    fn toggle_round_trips_and_language_names_are_local() {
        assert_eq!(Language::En.toggle(), Language::PtBr);
        assert_eq!(Language::PtBr.toggle(), Language::En);
        assert_eq!(EN.switch_language, "Português (Brasil)");
        assert_eq!(PT.switch_language, "English");
    }

    #[test]
    fn statuses_and_reasons_keep_wire_values_out_of_the_display_map() {
        assert_eq!(status_label(Language::PtBr, "DEGRADED"), "DEGRADADO");
        assert_eq!(
            reason_label(Language::PtBr, "lease denied: lease expired"),
            "autorização expirada"
        );
        assert_eq!(status_label(Language::En, "ACTIVE"), "ACTIVE");
    }

    #[test]
    fn dashboard_phrases_and_operation_labels_cover_en_and_pt_br() {
        use crate::operation_progress::OperationKind;

        assert_eq!(Language::parse("en"), Some(Language::En));
        assert_eq!(Language::parse("pt-BR"), Some(Language::PtBr));
        assert_eq!(Language::parse("xx"), None);
        assert_eq!(Language::En.code(), "en");
        assert_eq!(Language::PtBr.code(), "pt-BR");

        assert_eq!(pressure_class(0.2), "LOW");
        assert_eq!(pressure_class(1.5), "ATTENTION");
        assert_eq!(pressure_class(3.0), "GUARDED");
        assert_eq!(pressure_class(6.0), "CRITICAL");
        assert_eq!(pressure_class(12.0), "EMERGENCY");
        for class in [
            "LOW",
            "ATTENTION",
            "GUARDED",
            "CRITICAL",
            "EMERGENCY",
            "OTHER",
        ] {
            for lang in [Language::En, Language::PtBr] {
                assert!(!pressure_label(lang, class).is_empty());
            }
        }
        assert_eq!(
            reason_label(Language::PtBr, "safe mode after disconnect"),
            "modo seguro após desconexão"
        );
        assert_eq!(
            reason_label(Language::En, "requires a live vsock lease"),
            "no host lease"
        );

        for phrase in [
            DashboardPhrase::ReadLabel,
            DashboardPhrase::WriteLabel,
            DashboardPhrase::BatchWrites,
            DashboardPhrase::GpuCodecName,
            DashboardPhrase::SlabLabel,
            DashboardPhrase::SavedLabel,
            DashboardPhrase::PagesOneToOne,
            DashboardPhrase::CodecAwaitingTier2,
            DashboardPhrase::Tier2SwapLabel,
            DashboardPhrase::CompressedBytesLabel,
        ] {
            for lang in [Language::En, Language::PtBr] {
                assert!(!dashboard_phrase(lang, phrase).is_empty());
            }
        }

        for kind in [
            OperationKind::Teardown,
            OperationKind::SwapDrain,
            OperationKind::SwapRebalance,
            OperationKind::CascadeUp,
            OperationKind::SetupInstall,
            OperationKind::ReleaseSync,
            OperationKind::StorageBenchmark,
            OperationKind::StressTest,
        ] {
            for lang in [Language::En, Language::PtBr] {
                assert!(!operation_kind_label(lang, kind).is_empty());
            }
        }

        for phase in [
            "prepare",
            "swapoff",
            "reset_zram",
            "disconnect_nbd",
            "stop_daemon",
            "origin_check",
            "zram_setup",
            "daemon_start",
            "nbd_attach",
            "zram_bind",
            "prepare_storage",
            "commit_config",
            "verify",
            "observe_fleet",
            "restart_services",
            "benchmark_volume",
            "stress_ramp",
            "stress_verify",
            "custom_unmapped_phase",
        ] {
            for lang in [Language::En, Language::PtBr] {
                assert!(!operation_phase_label(lang, phase).is_empty());
            }
        }
        assert_eq!(
            operation_phase_label(Language::En, "stress_ramp"),
            "allocating & spilling across tiers"
        );
        assert_eq!(
            operation_phase_label(Language::PtBr, "stress_verify"),
            "validando leitura bit-exata e cache GPU"
        );
    }
}
