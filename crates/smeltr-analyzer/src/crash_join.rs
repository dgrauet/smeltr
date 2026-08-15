//! Analyze-time crash-report join (#153).
//!
//! ReportCrash writes the `.ips` seconds AFTER the crashed process dies —
//! by then the scoped session is already finalized (the record client's
//! connection dropped, #143), so the live crash-reports probe cannot land
//! the report in the crashed session. This module joins retroactively:
//! given the crashed session's child pid and wall-clock window, it scans
//! the DiagnosticReports directory for a matching report and turns it
//! into a RootCause finding. Works on sessions recorded before the fix.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use smeltr_core::event::{Event, Payload};
use smeltr_probes_crash_reports::parse::parse_ips;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// How long after the session end a report may be written and still be
/// attributed to it. ReportCrash typically takes seconds; sleep/wake and
/// symbolication can stretch that.
pub const CRASH_REPORT_GRACE_NS: u64 = 120_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashJoin {
    pub path: String,
    pub crashed_pid: u32,
    pub signal: Option<String>,
    pub summary: String,
    pub exception_codes: Vec<String>,
}

/// Scan `reports_dir` for a `.ips` whose crashed pid matches `pid` and
/// whose mtime falls inside `[wall_start_ns, wall_end_ns + grace_ns]`
/// (unix wall-clock ns). Returns the newest match.
pub fn find_crash_report(
    reports_dir: &Path,
    pid: u32,
    wall_start_ns: u64,
    wall_end_ns: u64,
    grace_ns: u64,
) -> Option<CrashJoin> {
    let entries = std::fs::read_dir(reports_dir).ok()?;
    let mut best: Option<(u64, CrashJoin)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ips") {
            continue;
        }
        let mtime_ns = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64);
        let Some(mtime_ns) = mtime_ns else { continue };
        if mtime_ns < wall_start_ns || mtime_ns > wall_end_ns.saturating_add(grace_ns) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(Payload::CrashReportEmitted {
            path: p,
            crashed_pid,
            signal,
            exception_codes,
            summary,
        }) = parse_ips(&content, &path.to_string_lossy())
        else {
            continue;
        };
        if crashed_pid != Some(pid) {
            continue;
        }
        let join = CrashJoin {
            path: p,
            crashed_pid: pid,
            signal,
            summary,
            exception_codes,
        };
        match &best {
            Some((t, _)) if *t >= mtime_ns => {}
            _ => best = Some((mtime_ns, join)),
        }
    }
    best.map(|(_, j)| j)
}

/// Turn a joined crash report into a RootCause finding for the report.
pub fn crash_finding(j: &CrashJoin) -> Finding {
    let title = match &j.signal {
        Some(sig) => format!("Recorded process crashed ({sig})"),
        None => "Recorded process crashed".to_string(),
    };
    let mut detail = String::new();
    if !j.summary.is_empty() {
        detail.push_str(&j.summary);
    }
    if !j.exception_codes.is_empty() {
        if !detail.is_empty() {
            detail.push_str(" — ");
        }
        detail.push_str(&format!("codes: {}", j.exception_codes.join(", ")));
    }
    if !detail.is_empty() {
        detail.push_str("\n    ");
    }
    detail.push_str(&format!("crash report: {}", j.path));
    Finding::new(Severity::Critical, Category::RootCause, title).with_detail(detail)
}

/// Un kill jetsam joint rétroactivement à la session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JetsamJoin {
    pub path: String,
    pub killed_pid: u32,
    pub killed_name: String,
    pub footprint_bytes: u64,
    pub lifetime_max_bytes: u64,
    /// Motif rendu par le noyau, quand il le donne.
    pub reason: Option<String>,
}

/// Répertoires où macOS dépose ses rapports de diagnostic.
///
/// Le répertoire SYSTÈME vient en premier : c'est là que les
/// `JetsamEvent-*.ips` sont écrits, contrairement aux rapports de crash
/// ordinaires qui vont dans le répertoire utilisateur. Ni la sonde ni la
/// jointure de crash ne regardaient le répertoire système — sans ça la
/// fonctionnalité ne se déclenche jamais.
///
/// `SMELTR_DIAGNOSTIC_REPORTS_DIR` remplace toute la liste, pour les tests.
pub fn diagnostic_reports_dirs() -> Vec<std::path::PathBuf> {
    if let Some(over) = std::env::var_os("SMELTR_DIAGNOSTIC_REPORTS_DIR") {
        return vec![std::path::PathBuf::from(over)];
    }
    let mut dirs = vec![std::path::PathBuf::from("/Library/Logs/DiagnosticReports")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join("Library/Logs/DiagnosticReports"));
    }
    dirs
}

/// Deux noms de processus désignent-ils plausiblement le même processus ?
///
/// Comparaison par PRÉFIXE, pas par égalité : les deux côtés tronquent
/// différemment. `pbi_comm` (d'où viennent les noms de `ProcFootprint`) fait
/// 16 octets — `MAXCOMLEN` — tandis que le `name` d'un rapport jetsam va
/// jusqu'à ~32 (observé sur cette machine :
/// `"com.apple.Virtualization.Virtual"`, 32 caractères). Exiger l'égalité
/// rejetterait précisément les noms longs.
///
/// Un nom vide n'apporte aucune information et ne prouve donc rien.
pub fn names_compatible(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len());
    n > 0 && a.as_bytes()[..n] == b.as_bytes()[..n]
}

/// Cherche dans `dirs` un rapport jetsam nommant l'un de `pids`, dont le
/// mtime tombe dans `[wall_start_ns, wall_end_ns + grace_ns]`. Retourne le
/// plus récent.
///
/// `pids` contient le PID scopé ET tout PID observé dans les échantillons
/// d'empreinte : sous `uv run` / `poetry run` / `python -m`, le processus
/// qui meurt est un petit-enfant au PID différent (#31).
///
/// Le triple filtre PID + fenêtre + nom est ce qui empêche d'attribuer au run
/// analysé un kill jetsam d'un processus sans rapport avec lui. `known_names`
/// liste les noms connus du processus tracé ; vide, la garde de nom ne
/// s'applique pas et on retombe sur PID + fenêtre.
pub fn find_jetsam_report(
    dirs: &[std::path::PathBuf],
    pids: &[u32],
    known_names: &[String],
    wall_start_ns: u64,
    wall_end_ns: u64,
    grace_ns: u64,
) -> Option<JetsamJoin> {
    let mut best: Option<(u64, JetsamJoin)> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ips") {
                continue;
            }
            let mtime_ns = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64);
            let Some(mtime_ns) = mtime_ns else { continue };
            if mtime_ns < wall_start_ns || mtime_ns > wall_end_ns.saturating_add(grace_ns) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(Payload::JetsamKill {
                path: p,
                killed_pid,
                killed_name,
                footprint_bytes,
                lifetime_max_bytes,
                reason,
                ..
            }) = parse_ips(&content, &path.to_string_lossy())
            else {
                continue;
            };
            let Some(matched_pid) = killed_pid.filter(|k| pids.contains(k)) else {
                continue;
            };
            // Garde de nom : le PID seul ne suffit pas, macOS les recycle.
            // Elle ne s'applique que si les DEUX côtés donnent un nom —
            // sinon rejeter ferait rater le kill qu'on cherche.
            if !known_names.is_empty()
                && !killed_name.is_empty()
                && !known_names
                    .iter()
                    .any(|n| names_compatible(n, &killed_name))
            {
                continue;
            }
            let join = JetsamJoin {
                path: p,
                killed_pid: matched_pid,
                killed_name,
                footprint_bytes,
                lifetime_max_bytes,
                reason,
            };
            match &best {
                Some((t, _)) if *t >= mtime_ns => {}
                _ => best = Some((mtime_ns, join)),
            }
        }
    }
    best.map(|(_, j)| j)
}

/// Transforme un kill joint en cause racine.
pub fn jetsam_finding(j: &JetsamJoin) -> Finding {
    // Go décimal (1e9), pas Gio binaire — cohérent avec la façon dont macOS
    // et les rapports jetsam eux-mêmes expriment les empreintes mémoire.
    let gb = |b: u64| b as f64 / 1_000_000_000.0;
    // `per-process-limit` et `vm-pageshortage` appellent des corrections
    // opposées : réduire l'empreinte du run vs libérer la machine. Le motif
    // est dans le rapport ; le taire, c'est jeter la réponse à la question
    // même que cette fonctionnalité existe pour trancher.
    let motif = match j.reason.as_deref() {
        Some("per-process-limit") => {
            " Motif : `per-process-limit` — le processus a dépassé SA propre \
             limite, indépendamment de l'état du reste de la machine."
                .to_string()
        }
        Some("vm-pageshortage") => {
            " Motif : `vm-pageshortage` — c'est la machine entière qui manquait \
             de mémoire ; le run n'est pas forcément le fautif."
                .to_string()
        }
        Some(other) => format!(" Motif rendu par le noyau : `{other}`."),
        None => String::new(),
    };
    Finding::new(
        Severity::Critical,
        Category::RootCause,
        "Le noyau a tué le processus enregistré sous pression mémoire (jetsam)",
    )
    .with_detail(format!(
        "jetsam a tué le PID {} ({}) avec une empreinte de {:.2} Go \
         (maximum de vie {:.2} Go).{} C'est `phys_footprint` qui décide, pas la \
         mémoire MTLDevice : un run peut tenir dans le budget GPU et se faire \
         tuer quand même. Le processus disparaît sans traceback ni exception — \
         ce finding est la seule trace de la décision.\n    rapport : {}",
        j.killed_pid,
        j.killed_name,
        gb(j.footprint_bytes),
        gb(j.lifetime_max_bytes),
        motif,
        j.path
    ))
}

/// Joint un éventuel kill jetsam au rapport, en tête des findings.
///
/// Appelée à la fois par `smeltr analyze` et par le MCP `get_session_summary` :
/// un chiffre qu'on ne peut pas interroger depuis une session Claude ne sert à
/// rien. Sans effet sur les sessions ambiantes (pas de PID à joindre).
pub fn join_jetsam(report: &mut crate::report::Report, dir: &Path) {
    let Ok(meta) = smeltr_core::reader::read_metadata(dir) else {
        return;
    };
    let smeltr_core::session::SessionKind::Scoped { pid, argv } = &meta.kind else {
        return;
    };
    let Some(start_ns) = rfc3339_unix_ns(&meta.started_rfc3339) else {
        return;
    };
    let events = smeltr_core::reader::read_events(dir).unwrap_or_default();

    let end_ns = window_end_ns(&meta, &events);

    // PID candidats : le PID scopé PLUS tout PID vu dans les échantillons
    // d'empreinte, qui couvrent tout l'arbre tracé. Sous `uv run` /
    // `poetry run` / `python -m` — le flux normal du projet — le processus
    // qui meurt est un petit-enfant au PID différent de l'enfant lancé
    // (#31) : s'en tenir au PID scopé rendait le silence dans le cas même
    // que la fonctionnalité vise.
    //
    // Noms connus, pour la garde de nom : le basename d'argv[0] et tout nom
    // vu dans ces mêmes échantillons.
    let mut pids: Vec<u32> = vec![*pid];
    let mut known_names: Vec<String> = argv
        .first()
        .and_then(|a| a.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();
    for e in &events {
        if let Payload::ProcFootprint {
            pid: sample_pid,
            name,
            ..
        } = &e.payload
        {
            if !pids.contains(sample_pid) {
                pids.push(*sample_pid);
            }
            if !name.is_empty() && !known_names.iter().any(|n| n == name) {
                known_names.push(name.clone());
            }
        }
    }

    // Volontairement PAS de garde sur meta.exit_code (contrairement au join
    // de crash) : un processus tué par jetsam n'a pas de code de sortie
    // propre, mais le shell parent peut en rapporter un — on ne veut pas
    // rater le kill pour ça. Le triple filtre PID + fenêtre + nom suffit à
    // éviter les faux positifs.
    if let Some(j) = find_jetsam_report(
        &diagnostic_reports_dirs(),
        &pids,
        &known_names,
        start_ns,
        end_ns,
        CRASH_REPORT_GRACE_NS,
    ) {
        report.findings.insert(0, jetsam_finding(&j));
        return;
    }

    // Pas de rapport joignable : reste la présomption, et seulement ici.
    // La suppression est structurelle — on n'atteint ce point que faute de
    // verdict — plutôt qu'une vérification séparée qu'on pourrait oublier.
    if let Some(f) = presume_memory_death(&meta, &events, report) {
        report.findings.push(f);
    }
}

/// Borne haute de la fenêtre wall-clock d'une session, pour joindre un
/// rapport écrit après coup.
///
/// Un kill (jetsam comme crash) empêche souvent le client `record` de
/// finaliser proprement (#143) : à défaut d'`ended_rfc3339`, on borne au
/// dernier événement écrit. Laisser courir jusqu'à MAINTENANT rendrait la
/// fenêtre large de plusieurs semaines sur une vieille session non
/// finalisée — et il ne resterait alors qu'un PID que macOS recycle pour
/// garder le verdict. Repli sur maintenant seulement s'il n'y a aucun
/// événement à dater.
fn window_end_ns(meta: &smeltr_core::session::SessionMetadata, events: &[Event]) -> u64 {
    meta.ended_rfc3339
        .as_deref()
        .and_then(rfc3339_unix_ns)
        .or_else(|| {
            events
                .iter()
                .map(|e| e.ts_wall_ns)
                .max()
                .map(|t| t.saturating_add(CRASH_REPORT_GRACE_NS))
        })
        .unwrap_or_else(|| time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u64)
}

/// Joint un éventuel rapport de crash à la session, en tête des findings.
///
/// Jumeau de [`join_jetsam`], appelé depuis `smeltr analyze` **et** depuis le
/// MCP `get_session_summary`. Avant #204 ce travail vivait dans `analyze.rs`
/// et n'existait donc qu'en CLI : le MCP ne rendait jamais le verdict d'un
/// crash, et depuis #201 il lui substituait la présomption de mort mémoire —
/// dire quelque chose d'inexact étant pire que se taire.
///
/// La fenêtre passe par [`window_end_ns`], donc une session jamais finalisée
/// est couverte elle aussi ; l'ancien bloc exigeait `ended_rfc3339` et sautait
/// silencieusement ce cas, y compris en CLI.
pub fn join_crash(report: &mut crate::report::Report, dir: &Path) {
    let Ok(meta) = smeltr_core::reader::read_metadata(dir) else {
        return;
    };
    let smeltr_core::session::SessionKind::Scoped { pid, .. } = &meta.kind else {
        return;
    };
    // Une sortie propre n'est pas un crash. Garde volontairement absente de
    // `join_jetsam` (un kill jetsam peut laisser le shell rapporter un code
    // propre), mais légitime ici : ReportCrash n'écrit rien sur un exit 0.
    if meta.exit_code == Some(0) {
        return;
    }
    let Some(start_ns) = rfc3339_unix_ns(&meta.started_rfc3339) else {
        return;
    };
    let events = smeltr_core::reader::read_events(dir).unwrap_or_default();
    let end_ns = window_end_ns(&meta, &events);

    for reports_dir in diagnostic_reports_dirs() {
        if let Some(j) =
            find_crash_report(&reports_dir, *pid, start_ns, end_ns, CRASH_REPORT_GRACE_NS)
        {
            report.findings.insert(0, crash_finding(&j));
            return;
        }
    }
}

/// Croissance minimale entre le premier et le dernier échantillon pour
/// qualifier une pente. Facteur, pas seuil absolu : on ne prétend pas savoir
/// à partir de quelle taille le noyau frappe, et la limite jetsam par
/// processus n'est de toute façon pas interrogeable sans droits particuliers.
const RISE_FACTOR: f64 = 2.0;

/// Présume une mort par pression mémoire quand aucun rapport n'est joignable.
///
/// Trois conditions, toutes nécessaires :
///
/// 1. **La session s'est terminée, et anormalement.** C'est le code de sortie
///    qui le dit, pas le flux d'événements : `finalize()` écrit `SessionEnded`
///    même quand l'enfant a été tué, parce que le client `record` survit au
///    kill et détache proprement (#201). `Some(-1)` vient de
///    `status.code().unwrap_or(-1)` et signifie « tué par un signal ». Une
///    sortie propre (`Some(0)`) comme un échec ordinaire (`Some(1)`) sont
///    exclus.
///
///    Le cas `None` demande une garde supplémentaire. Il couvre deux formes
///    de fin anormale — le daemon a finalisé sur déconnexion du client, ou la
///    reprise au boot a rattrapé une session orpheline — mais aussi une
///    session **encore en cours**, dont le code de sortie n'existe pas
///    encore. Sans distinction, un `smeltr analyze --last` pendant un long
///    run annoncerait une fin anormale sur une session qui se porte bien.
///    Or tout chemin de finalisation écrit `ended_rfc3339` : `finalize()` le
///    pose avec le code de sortie (`writer.rs`), et la reprise au boot le
///    pose aussi (`recovery.rs`). Son absence signifie donc exactement
///    « session vivante », et c'est ce qu'on exclut.
/// 2. **L'empreinte montait.** Le premier échantillon est le tout premier tic
///    de la sonde, avant que l'enfant ait exec'é sa charge, donc la pente
///    seule ne prouve rien — d'où la condition 1.
/// 3. **Aucune cause racine n'est déjà établie.** Un SIGSEGV donne aussi
///    `-1` ; s'il a été joint en amont, le verdict prime sur la supposition.
fn presume_memory_death(
    meta: &smeltr_core::session::SessionMetadata,
    events: &[Event],
    report: &crate::report::Report,
) -> Option<Finding> {
    let ended_abnormally = match meta.exit_code {
        Some(-1) => true,
        // Pas de code de sortie : fin anormale seulement si la session est
        // bel et bien terminée. Sinon elle est en cours — voir ci-dessus.
        None => meta.ended_rfc3339.is_some(),
        _ => false,
    };
    if !ended_abnormally {
        return None;
    }
    if report
        .findings
        .iter()
        .any(|f| f.category == Category::RootCause)
    {
        return None;
    }

    let samples: Vec<(&Event, u64)> = events
        .iter()
        .filter_map(|e| match &e.payload {
            Payload::ProcFootprint {
                phys_footprint_bytes,
                is_traced_root: true,
                ..
            } => Some((e, *phys_footprint_bytes)),
            _ => None,
        })
        .collect();
    let (first_ev, first) = samples.first().copied()?;
    let (last_ev, last) = samples.last().copied()?;
    if first == 0 || (last as f64) < (first as f64) * RISE_FACTOR {
        return None;
    }

    // Go décimal (1e9), comme `jetsam_finding` : macOS et les rapports jetsam
    // expriment les empreintes ainsi, et deux findings sur le même fait dans
    // deux unités différentes ne se comparent pas.
    let gb = |b: u64| b as f64 / 1_000_000_000.0;
    Some(
        Finding::new(
            Severity::Warning,
            Category::ContributingFactor,
            "L'empreinte mémoire montait et la session s'est terminée anormalement",
        )
        .with_detail(format!(
            "L'empreinte du processus tracé est passée de {:.2} à {:.2} Go, \
             et la session ne s'est pas terminée proprement, sans qu'aucun \
             rapport jetsam ne soit joignable. Une mort par pression mémoire \
             est possible : le processus disparaît alors sans traceback ni \
             exception. Vérifiez /Library/Logs/DiagnosticReports pour un \
             JetsamEvent-*.ips postérieur à la session — il peut être écrit \
             après coup. Ceci est une présomption, pas un verdict.",
            gb(first),
            gb(last)
        ))
        .with_evidence(EvidenceRef {
            seq: first_ev.seq,
            ts_mono_ns: first_ev.ts_mono_ns,
            description: format!("empreinte initiale {:.2} Go", gb(first)),
        })
        .with_evidence(EvidenceRef {
            seq: last_ev.seq,
            ts_mono_ns: last_ev.ts_mono_ns,
            description: format!("dernière empreinte {:.2} Go", gb(last)),
        }),
    )
}

/// Même parsing que `analyze.rs` : les timestamps des métadonnées sont du
/// vrai wall-clock, contrairement aux `ts_wall_ns` des événements qui
/// dérivent de l'horloge monotone et s'arrêtent en veille (#153).
pub fn rfc3339_unix_ns(s: &str) -> Option<u64> {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::parse(s, &Rfc3339).ok()?;
    u64::try_from(t.unix_timestamp_nanos()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTILINE: &str =
        include_str!("../../smeltr-probes-crash-reports/tests/fixtures/sample_multiline.ips");

    /// Window around the fixture file's mtime (files are written by the
    /// test itself, so mtime is "now").
    fn window_around(path: &Path) -> (u64, u64) {
        let mtime = std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        (mtime.saturating_sub(60_000_000_000), mtime)
    }

    #[test]
    fn joins_matching_pid_and_window() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python-2026-07-16-213821.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        let (start, end) = window_around(&f);
        let j = find_crash_report(tmp.path(), 11672, start, end, CRASH_REPORT_GRACE_NS)
            .expect("no join");
        assert_eq!(j.crashed_pid, 11672);
        assert_eq!(j.signal.as_deref(), Some("SIGABRT"));
        assert!(j.summary.contains("EXC_CRASH"));
    }

    #[test]
    fn pid_mismatch_yields_none() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        let (start, end) = window_around(&f);
        assert!(find_crash_report(tmp.path(), 999, start, end, CRASH_REPORT_GRACE_NS).is_none());
    }

    #[test]
    fn report_outside_window_plus_grace_yields_none() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        // Session ended an hour before the report was written.
        let (start, end) = window_around(&f);
        let (old_start, old_end) = (
            start.saturating_sub(3_600_000_000_000),
            end.saturating_sub(3_600_000_000_000),
        );
        assert!(
            find_crash_report(tmp.path(), 11672, old_start, old_end, CRASH_REPORT_GRACE_NS)
                .is_none()
        );
    }

    #[test]
    fn missing_dir_yields_none() {
        assert!(
            find_crash_report(Path::new("/nonexistent-dir-xyz"), 11672, 0, u64::MAX / 2, 0)
                .is_none()
        );
    }

    const JETSAM: &str =
        include_str!("../../smeltr-probes-crash-reports/tests/fixtures/jetsam.ips");

    #[test]
    fn joins_jetsam_report_by_pid_and_window() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();
        let (start, end) = window_around(&f);
        let j = find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[4242],
            &[],
            start,
            end,
            CRASH_REPORT_GRACE_NS,
        )
        .expect("no join");
        assert_eq!(j.killed_pid, 4242);
        assert_eq!(j.killed_name, "python");
        // 1 310 720 pages × 16 Ko = 21,47 Go — la signature du bug ltx-2-mlx #74.
        assert_eq!(j.footprint_bytes, 1_310_720 * 16_384);
    }

    /// Le garde-fou contre la fausse cause racine : un kill jetsam d'un AUTRE
    /// processus ne doit jamais être attribué au run analysé.
    #[test]
    fn does_not_join_jetsam_report_of_another_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();
        let (start, end) = window_around(&f);
        assert!(find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[999_999],
            &[],
            start,
            end,
            CRASH_REPORT_GRACE_NS
        )
        .is_none());
    }

    /// Second garde-fou : hors de la fenêtre wall-clock, pas de jointure.
    #[test]
    fn does_not_join_jetsam_report_outside_window() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();
        let (start, _end) = window_around(&f);
        // Fenêtre fermée bien avant l'écriture du fichier.
        assert!(find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[4242],
            &[],
            start.saturating_sub(600_000_000_000),
            start.saturating_sub(300_000_000_000),
            0
        )
        .is_none());
    }

    /// Un rapport de crash ordinaire n'est pas un rapport jetsam.
    #[test]
    fn regular_crash_report_is_not_joined_as_jetsam() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python-2026-07-16-213821.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        let (start, end) = window_around(&f);
        assert!(find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[11672],
            &[],
            start,
            end,
            CRASH_REPORT_GRACE_NS
        )
        .is_none());
    }

    /// Le répertoire SYSTÈME doit être dans la liste : c'est là que macOS
    /// écrit les JetsamEvent-*.ips. Vérifié sur la machine : 0 fichier jetsam
    /// dans ~/Library/Logs/DiagnosticReports, 1 dans /Library/Logs/DiagnosticReports.
    #[test]
    #[serial_test::serial]
    fn jetsam_dirs_include_the_system_directory() {
        let dirs = diagnostic_reports_dirs();
        assert!(
            dirs.iter()
                .any(|d| d == std::path::Path::new("/Library/Logs/DiagnosticReports")),
            "dirs: {dirs:?}"
        );
    }

    #[test]
    fn jetsam_finding_is_a_critical_root_cause() {
        let j = JetsamJoin {
            path: "/x/JetsamEvent.ips".into(),
            killed_pid: 4242,
            killed_name: "python".into(),
            footprint_bytes: 21_474_836_480,
            lifetime_max_bytes: 21_474_836_480,
            reason: Some("per-process-limit".into()),
        };
        let f = jetsam_finding(&j);
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.category, Category::RootCause);
        assert!(f.detail.contains("21"), "detail: {}", f.detail);
        assert!(f.detail.contains("jetsam") || f.title.contains("jetsam"));
        // Le POURQUOI du kill doit être rendu : c'est la question à laquelle
        // la fonctionnalité existe pour répondre.
        assert!(
            f.detail.contains("per-process-limit"),
            "detail: {}",
            f.detail
        );
    }

    /// Sans motif rendu par le noyau, le finding reste lisible : pas de
    /// « Motif : None » ni de phrase amputée.
    #[test]
    fn jetsam_finding_without_a_reason_stays_clean() {
        let j = JetsamJoin {
            path: "/x/JetsamEvent.ips".into(),
            killed_pid: 4242,
            killed_name: "python".into(),
            footprint_bytes: 21_474_836_480,
            lifetime_max_bytes: 21_474_836_480,
            reason: None,
        };
        let f = jetsam_finding(&j);
        assert!(!f.detail.contains("Motif"), "detail: {}", f.detail);
        assert!(!f.detail.contains("None"), "detail: {}", f.detail);
    }

    #[test]
    #[serial_test::serial]
    fn join_jetsam_inserts_the_finding_for_a_scoped_session() {
        use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
        use smeltr_core::writer::SessionWriter;

        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // La session démarre avant que le rapport jetsam ne soit écrit,
        // comme dans la vraie chronologie (#153) : le kill puis le rapport
        // arrivent après le début de la session.
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = SessionKind::Scoped {
            pid: 4242,
            argv: vec![],
        };
        let dir = {
            let w = SessionWriter::create(meta).unwrap();
            let d = w.dir().to_path_buf();
            drop(w);
            d
        };

        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);

        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");
        assert_eq!(report.findings.len(), 1, "findings: {:#?}", report.findings);
        assert_eq!(report.findings[0].category, Category::RootCause);
    }

    /// Construit une session scopée sur disque : métadonnées (départ, fin,
    /// argv) et flux d'événements maîtrisés, ce qu'aucun helper existant ne
    /// permet — `SessionWriter::create` date toujours le départ à maintenant.
    fn scoped_session(
        home: &Path,
        pid: u32,
        argv: Vec<String>,
        started: &str,
        ended: Option<&str>,
        events: &[smeltr_core::event::Event],
    ) -> std::path::PathBuf {
        use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
        use smeltr_core::writer::SessionWriter;

        // SMELTR_HOME est déjà positionné par l'appelant (test #[serial]).
        let _ = home;
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = SessionKind::Scoped { pid, argv };
        let mut w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        for e in events {
            w.write_event(e).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        // Réécrit les horodatages APRÈS coup : le writer impose "maintenant".
        let mut meta = smeltr_core::reader::read_metadata(&dir).unwrap();
        meta.started_rfc3339 = started.to_string();
        meta.ended_rfc3339 = ended.map(str::to_string);
        smeltr_core::session::write_metadata(&dir, &meta).unwrap();
        dir
    }

    fn footprint_ev(
        seq: u64,
        ts_wall_ns: u64,
        pid: u32,
        name: &str,
        is_traced_root: bool,
    ) -> smeltr_core::event::Event {
        smeltr_core::event::Event {
            ts_mono_ns: seq,
            ts_wall_ns,
            session_id: uuid::Uuid::nil(),
            source: smeltr_core::event::Source::Proc,
            pid: Some(pid),
            seq,
            payload: Payload::ProcFootprint {
                pid,
                name: name.into(),
                phys_footprint_bytes: 1_000_000,
                lifetime_max_phys_footprint_bytes: 1_000_000,
                is_traced_root,
            },
        }
    }

    fn now_ns() -> u64 {
        time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u64
    }

    /// Le bord non borné : une session jamais finalisée — exactement ce que
    /// jetsam produit — voyait sa fenêtre courir jusqu'à MAINTENANT. Une
    /// session de mai analysée aujourd'hui avale ainsi tout rapport jetsam
    /// des semaines suivantes, sur la seule foi d'un PID que macOS recycle.
    #[test]
    #[serial_test::serial]
    fn unfinalized_session_does_not_swallow_a_much_later_report() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Dernier événement il y a une heure ; la session n'a jamais été
        // finalisée (pas d'`ended_rfc3339`), comme après un kill jetsam.
        let hour_ago = now_ns().saturating_sub(3_600_000_000_000);
        let dir = scoped_session(
            home.path(),
            4242,
            vec!["python".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, hour_ago, 4242, "python", true)],
        );

        // Le rapport, lui, est écrit MAINTENANT : bien après la fin réelle
        // de la session.
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(
            report.findings.is_empty(),
            "fenêtre non bornée : {:#?}",
            report.findings
        );
    }

    /// Le repli borné reste utile : un rapport écrit juste après le dernier
    /// événement d'une session non finalisée doit toujours être joint.
    #[test]
    #[serial_test::serial]
    fn unfinalized_session_still_joins_a_report_written_right_after() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let dir = scoped_session(
            home.path(),
            4242,
            vec!["python".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, now_ns(), 4242, "python", true)],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    }

    /// Le PID seul ne suffit pas : macOS les recycle. Quand les deux côtés
    /// donnent un nom et qu'ils divergent, pas de jointure.
    #[test]
    #[serial_test::serial]
    fn name_mismatch_rejects_the_join() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Le rapport nomme "python" ; cette session-là est un run ruby qui
        // a simplement réutilisé le PID 4242.
        let dir = scoped_session(
            home.path(),
            4242,
            vec!["/usr/bin/ruby".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, now_ns(), 4242, "ruby", true)],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(
            report.findings.is_empty(),
            "nom incompatible joint quand même : {:#?}",
            report.findings
        );
    }

    /// Les deux côtés tronquent différemment : `pbi_comm` fait 16 octets
    /// (MAXCOMLEN), le `name` d'un rapport jetsam ~32 (observé sur cette
    /// machine : "com.apple.Virtualization.Virtual", 32 caractères). Comparer
    /// à égalité rejetterait le vrai cas — on compare par préfixe.
    #[test]
    fn truncated_names_match_by_prefix() {
        assert!(names_compatible(
            "com.apple.Virtua",
            "com.apple.Virtualization.Virtual"
        ));
        assert!(names_compatible("python", "python"));
        assert!(!names_compatible("ruby", "python"));
        // Un nom vide n'apporte aucune information : il ne prouve rien.
        assert!(!names_compatible("", "python"));
    }

    /// Sans nom d'un côté, on ne rejette pas : PID + fenêtre restent les
    /// gardes, comme avant. Rejeter ferait rater le kill que la
    /// fonctionnalité existe pour attraper.
    #[test]
    #[serial_test::serial]
    fn missing_name_falls_back_to_pid_and_window() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Ni argv ni ProcFootprint : aucun nom côté session.
        let dir = scoped_session(
            home.path(),
            4242,
            vec![],
            "2026-05-15T17:35:05Z",
            Some(
                &time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            ),
            &[],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    }

    /// Le cas normal du projet, pas un cas limite : sous `uv run` /
    /// `poetry run` / `python -m`, le processus qui meurt est un
    /// petit-enfant dont le PID diffère de celui de l'enfant lancé — c'est
    /// la raison d'être de `SMELTR_SCOPE_TOKEN` (#31). Le rapport jetsam
    /// nomme le petit-enfant ; ne regarder que le PID scopé rendait le
    /// silence exactement dans le cas visé.
    #[test]
    #[serial_test::serial]
    fn joins_a_grandchild_pid_seen_in_footprint_samples() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // La session est scopée sur le lanceur (`uv`, PID 9999) ; le vrai
        // travail — et le kill — est sur le petit-enfant python 4242.
        let dir = scoped_session(
            home.path(),
            9999,
            vec!["/opt/homebrew/bin/uv".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[
                footprint_ev(1, now_ns(), 9999, "uv", true),
                footprint_ev(2, now_ns(), 4242, "python", false),
            ],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        assert!(
            report.findings[0].detail.contains("4242"),
            "detail: {}",
            report.findings[0].detail
        );
    }

    /// Élargir aux PID observés ne doit pas ouvrir la porte : un PID que la
    /// session n'a jamais vu reste sans jointure.
    #[test]
    #[serial_test::serial]
    fn an_unobserved_pid_still_does_not_join() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let dir = scoped_session(
            home.path(),
            9999,
            vec!["/opt/homebrew/bin/uv".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, now_ns(), 9999, "uv", true)],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    #[test]
    fn crash_finding_is_critical_root_cause() {
        let j = CrashJoin {
            path: "/x/Python.ips".into(),
            crashed_pid: 11672,
            signal: Some("SIGABRT".into()),
            summary: "EXC_CRASH".into(),
            exception_codes: vec!["0x0".into()],
        };
        let f = crash_finding(&j);
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.category, Category::RootCause);
        assert!(f.title.contains("SIGABRT"));
        assert!(f.detail.contains("/x/Python.ips"));
    }

    // ---- présomption de mort mémoire (#201) ----

    /// Variante de `footprint_ev` avec une empreinte choisie.
    fn footprint_bytes_ev(
        seq: u64,
        ts_wall_ns: u64,
        pid: u32,
        bytes: u64,
    ) -> smeltr_core::event::Event {
        let mut e = footprint_ev(seq, ts_wall_ns, pid, "python3", true);
        if let Payload::ProcFootprint {
            phys_footprint_bytes,
            lifetime_max_phys_footprint_bytes,
            ..
        } = &mut e.payload
        {
            *phys_footprint_bytes = bytes;
            *lifetime_max_phys_footprint_bytes = bytes;
        }
        e
    }

    fn set_exit_code(dir: &Path, code: Option<i32>) {
        let mut meta = smeltr_core::reader::read_metadata(dir).unwrap();
        meta.exit_code = code;
        smeltr_core::session::write_metadata(dir, &meta).unwrap();
    }

    fn empty_report() -> crate::report::Report {
        crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        }
    }

    /// Prépare une session scopée avec empreinte, fin et code de sortie
    /// choisis, puis joint. Retourne les findings produits.
    fn risk_case(
        home: &Path,
        first: u64,
        last: u64,
        ended: Option<&str>,
        exit_code: Option<i32>,
    ) -> Vec<Finding> {
        let t = now_ns();
        let evs = vec![
            footprint_bytes_ev(1, t, 4242, first),
            footprint_bytes_ev(2, t + 1_000_000_000, 4242, last),
        ];
        let dir = scoped_session(
            home,
            4242,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            ended,
            &evs,
        );
        set_exit_code(&dir, exit_code);
        let mut report = empty_report();
        join_jetsam(&mut report, &dir);
        report.findings
    }

    /// LE cas que #201 rouvre : l'enfant est tué par un signal, aucun rapport
    /// jetsam n'est joignable, et l'empreinte montait. La présomption sort.
    #[test]
    #[serial_test::serial]
    fn killed_by_signal_with_rising_footprint_yields_the_presumption() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(home.path(), 4_000_000_000, 18_000_000_000, None, Some(-1));
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(f.len(), 1, "findings: {f:#?}");
        assert_eq!(f[0].severity, Severity::Warning);
        assert_eq!(f[0].category, Category::ContributingFactor);
        assert!(
            f[0].detail.contains("possible") || f[0].detail.contains("peut"),
            "le ton doit rester une présomption : {}",
            f[0].detail
        );
    }

    /// Le défaut d'origine : un run SAIN qui alloue beaucoup et se termine
    /// proprement ne doit rien produire. C'est ce que la première version
    /// de la règle faisait crier sur tous les runs.
    #[test]
    #[serial_test::serial]
    fn clean_exit_with_rising_footprint_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(
            home.path(),
            15_000_000,
            1_007_000_000,
            Some("2026-08-09T10:05:00Z"),
            Some(0),
        );
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "un run sain ne doit rien produire : {f:#?}");
    }

    /// Un programme qui échoue normalement (exit 1) n'est pas une mort mémoire.
    #[test]
    #[serial_test::serial]
    fn ordinary_failure_exit_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(
            home.path(),
            4_000_000_000,
            18_000_000_000,
            Some("2026-08-09T10:05:00Z"),
            Some(1),
        );
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "{f:#?}");
    }

    /// Empreinte plate + mort par signal : rien à présumer côté mémoire.
    #[test]
    #[serial_test::serial]
    fn killed_by_signal_without_rising_footprint_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(home.path(), 4_000_000_000, 4_100_000_000, None, Some(-1));
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "{f:#?}");
    }

    /// Sans code de sortie mais AVEC une fin datée : le daemon a finalisé sur
    /// déconnexion du client, ou la reprise au boot a rattrapé une session
    /// orpheline. Fin anormale, la présomption sort.
    #[test]
    #[serial_test::serial]
    fn finalized_without_exit_code_counts_as_abnormal() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(
            home.path(),
            4_000_000_000,
            18_000_000_000,
            Some("2026-08-09T10:05:00Z"),
            None,
        );
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].severity, Severity::Warning);
    }

    /// #204 : la jointure de crash doit couvrir une session JAMAIS finalisée.
    /// L'ancien bloc de `analyze.rs` exigeait `ended_rfc3339` et sautait donc
    /// silencieusement ce cas, y compris en CLI — alors que c'est exactement
    /// la forme que produit une mort brutale.
    #[test]
    #[serial_test::serial]
    fn join_crash_covers_a_never_finalized_session() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Un événement daté maintenant : c'est lui qui borne la fenêtre en
        // l'absence de `ended_rfc3339`.
        let evs = vec![footprint_ev(1, now_ns(), 11672, "python3", true)];
        let dir = scoped_session(
            home.path(),
            11672,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            None,
            &evs,
        );
        set_exit_code(&dir, Some(-1));
        std::fs::write(reports.path().join("Python-crash.ips"), MULTILINE).unwrap();

        let mut report = empty_report();
        join_crash(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        assert_eq!(report.findings[0].category, Category::RootCause);
        assert!(report.findings[0].title.contains("crashed"));
    }

    /// Une sortie propre n'est pas un crash : garde conservée depuis
    /// `analyze.rs`.
    #[test]
    #[serial_test::serial]
    fn join_crash_skips_a_clean_exit() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let evs = vec![footprint_ev(1, now_ns(), 11672, "python3", true)];
        let dir = scoped_session(
            home.path(),
            11672,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            Some("2026-08-09T10:05:00Z"),
            &evs,
        );
        set_exit_code(&dir, Some(0));
        std::fs::write(reports.path().join("Python-crash.ips"), MULTILINE).unwrap();

        let mut report = empty_report();
        join_crash(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    /// Un crash prime sur la présomption mémoire : `join_crash` pose la cause
    /// racine, que `presume_memory_death` voit ensuite et respecte. C'est le
    /// scénario que le chemin MCP misattribuait avant #204.
    #[test]
    #[serial_test::serial]
    fn a_crash_suppresses_the_memory_presumption() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let t = now_ns();
        let evs = vec![
            footprint_bytes_ev(1, t, 11672, 4_000_000_000),
            footprint_bytes_ev(2, t + 1_000_000_000, 11672, 18_000_000_000),
        ];
        // `ended` absent : la fenêtre se borne au dernier événement, daté de
        // maintenant, donc elle couvre la mtime du rapport qu'on vient
        // d'écrire. Une date de fin figée dans le passé le mettrait hors
        // fenêtre et le test testerait autre chose.
        let dir = scoped_session(
            home.path(),
            11672,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            None,
            &evs,
        );
        set_exit_code(&dir, Some(-1));
        std::fs::write(reports.path().join("Python-crash.ips"), MULTILINE).unwrap();

        let mut report = empty_report();
        join_crash(&mut report, &dir);
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(
            report.findings.len(),
            1,
            "le crash doit primer, sans présomption ajoutée : {:#?}",
            report.findings
        );
        assert_eq!(report.findings[0].category, Category::RootCause);
    }

    /// Le faux positif à ne surtout pas laisser passer : une session ENCORE
    /// EN COURS n'a ni code de sortie ni fin datée, et son empreinte monte
    /// forcément. Un `smeltr analyze --last` pendant un long run ne doit pas
    /// annoncer une fin anormale sur une session qui se porte bien.
    #[test]
    #[serial_test::serial]
    fn live_in_progress_session_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Ni `ended`, ni `exit_code` : la forme exacte d'une session vivante.
        let f = risk_case(home.path(), 4_000_000_000, 18_000_000_000, None, None);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(
            f.is_empty(),
            "une session en cours ne s'est pas terminée anormalement : {f:#?}"
        );
    }

    /// Quand une cause racine est déjà établie (crash joint en amont), la
    /// présomption mémoire se tait : un verdict prime sur une supposition.
    #[test]
    #[serial_test::serial]
    fn existing_root_cause_suppresses_the_presumption() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let t = now_ns();
        let evs = vec![
            footprint_bytes_ev(1, t, 4242, 4_000_000_000),
            footprint_bytes_ev(2, t + 1_000_000_000, 4242, 18_000_000_000),
        ];
        let dir = scoped_session(
            home.path(),
            4242,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            None,
            &evs,
        );
        set_exit_code(&dir, Some(-1));

        let mut report = empty_report();
        report.findings.push(Finding::new(
            Severity::Critical,
            Category::RootCause,
            "Recorded process crashed (SIGSEGV)",
        ));
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        assert_eq!(report.findings[0].category, Category::RootCause);
    }
}
