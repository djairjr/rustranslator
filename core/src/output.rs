//! Where the translated document lands — and under what name.
//!
//! Naming rule (owner's decision, 2026-10-09):
//!
//! ```text
//! ORIGINAL.md   ->  ORIGINAL_pt-br.md
//! ORIGINAL.MD   ->  ORIGINAL_pt-br.MD
//! notas         ->  notas_pt-br
//! ```
//!
//! Underscore before the locale, extension preserved as-is, and the locale in **lower case**
//! in the file name. The configuration keeps BCP 47 (`pt-BR`); what lower-cases is the name,
//! not the configuration.
//!
//! The destination is decided in two places and the **call wins**: the tool/CLI parameter,
//! else the `doc_destination` from the configuration, else `junto_do_original`.

use std::path::{Path, PathBuf};

pub const NEXT_TO_ORIGINAL: &str = "next_to_original";

/// The name of the translated file, in the same directory as the original.
pub fn translated_name(source: &Path, locale: &str) -> PathBuf {
    let dir = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let locale = locale.to_ascii_lowercase();
    let nome = match source.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}_{locale}.{ext}"),
        None => format!("{stem}_{locale}"),
    };
    dir.join(nome)
}

/// The final path of the translated document.
///
/// `doc_destination` is the raw value from the call parameter or the configuration: empty or
/// `next_to_original` means "next to the original"; a path ending in a separator, or one that
/// is an existing directory, receives the generated name inside it; any other path is used
/// as-is.
///
/// It refuses to write over the source file itself: translating on top of the source destroys
/// the original.
pub fn resolve_destination(
    source: &Path,
    locale: &str,
    doc_destination: Option<&str>,
) -> Result<PathBuf, String> {
    let target = match doc_destination.map(str::trim).filter(|d| !d.is_empty()) {
        None | Some(NEXT_TO_ORIGINAL) => translated_name(source, locale),
        Some(d) => {
            let p = PathBuf::from(d);
            let eh_diretorio = d.ends_with('/') || p.is_dir();
            if eh_diretorio {
                let nome = translated_name(source, locale);
                p.join(nome.file_name().unwrap_or_default())
            } else {
                p
            }
        }
    };

    if same_path(&target, source) {
        return Err(format!(
            "o destino resolveria para o proprio arquivo de source ({}). Escolha outro \
             `doc_destination`: traduzir por cima da fonte destruiria o original.",
            source.display()
        ));
    }
    Ok(target)
}

/// Compares two paths by what they resolve to, without requiring them to exist.
fn same_path(a: &Path, b: &Path) -> bool {
    let abs = |p: &Path| -> PathBuf {
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        };
        // Normalise `..` and `.` without touching the disk.
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                outro => out.push(outro.as_os_str()),
            }
        }
        out
    };
    abs(a) == abs(b)
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn nome_segue_a_regra_do_dono() {
        assert_eq!(
            translated_name(Path::new("README.md"), "pt-BR"),
            PathBuf::from("README_pt-br.md")
        );
        assert_eq!(
            translated_name(Path::new("/x/ORIGINAL.MD"), "pt-BR"),
            PathBuf::from("/x/ORIGINAL_pt-br.MD")
        );
        assert_eq!(
            translated_name(Path::new("docs/guia.md"), "pt-BR"),
            PathBuf::from("docs/guia_pt-br.md")
        );
    }

    #[test]
    fn sem_extensao_tambem_funciona() {
        assert_eq!(
            translated_name(Path::new("notas"), "pt-BR"),
            PathBuf::from("notas_pt-br")
        );
    }

    #[test]
    fn outro_locale_nao_e_tratado_como_portugues() {
        assert_eq!(
            translated_name(Path::new("README.md"), "es-419"),
            PathBuf::from("README_es-419.md")
        );
    }

    #[test]
    fn padrao_e_ao_lado_do_original() {
        let p = resolve_destination(Path::new("/tmp/a/doc.md"), "pt-BR", None).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/a/doc_pt-br.md"));
        let p = resolve_destination(Path::new("/tmp/a/doc.md"), "pt-BR", Some(NEXT_TO_ORIGINAL))
            .unwrap();
        assert_eq!(p, PathBuf::from("/tmp/a/doc_pt-br.md"));
    }

    #[test]
    fn caminho_explicito_manda() {
        let p = resolve_destination(
            Path::new("/tmp/a/doc.md"),
            "pt-BR",
            Some("/out/pronto.md"),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/out/pronto.md"));
    }

    #[test]
    fn diretorio_com_barra_recebe_o_nome_gerado() {
        let p = resolve_destination(Path::new("/tmp/a/doc.md"), "pt-BR", Some("/out/")).unwrap();
        assert_eq!(p, PathBuf::from("/out/doc_pt-br.md"));
    }

    #[test]
    fn recusa_escrever_por_cima_da_fonte() {
        let e = resolve_destination(
            Path::new("/tmp/a/doc.md"),
            "pt-BR",
            Some("/tmp/a/doc.md"),
        )
        .unwrap_err();
        assert!(e.contains("source"), "error: {e}");
        // And also when the explicit path reaches the source by another route.
        let e = resolve_destination(
            Path::new("/tmp/a/doc.md"),
            "pt-BR",
            Some("/tmp/a/../a/doc.md"),
        )
        .unwrap_err();
        assert!(e.contains("source"), "error: {e}");
    }
}
