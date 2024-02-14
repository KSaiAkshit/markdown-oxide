use std::{hash::{DefaultHasher, Hash, Hasher}, path::{PathBuf, Path}, collections::HashSet};

use cached::proc_macro::cached;
use itertools::Itertools;
use nanoid::nanoid;
use nucleo_matcher::{pattern::{Normalization, self}, Matcher};
use rayon::prelude::*;
use serde::{Serialize, Deserialize};
use tower_lsp::{lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionParams,
    CompletionResponse, Documentation, CompletionTextEdit, TextEdit, Range, Position, CompletionList, Url, Command,
}, jsonrpc::Result, Client};

use crate::{
    ui::preview_referenceable,
    vault::{Preview, Referenceable, Vault, get_obsidian_ref_path, Block}, params_position_path,
};

fn get_link_index(line: &Vec<char>, cursor_character: usize) -> Option<usize> {
    line.get(0..=cursor_character)? // select only the characters up to the cursor
        .iter()
        .enumerate() // attach indexes
        .tuple_windows() // window into pairs of characters
        .collect::<Vec<(_, _)>>()
        .into_iter()
        .rev() // search from the cursor back
        .find(|((_,&c1), (_,&c2))| c1 == '[' && c2 == '[')
        .map(|(_, (i, _))| i) // only take the index; using map because find returns an option
}

pub fn get_completions(vault: &Vault, initial_completion_files: &[PathBuf], params: &CompletionParams, path: &Path) -> Option<CompletionResponse> {
    let Ok(path) = params
        .text_document_position
        .text_document
        .uri
        .to_file_path()
    else {
        return None;
    };

    let line = params.text_document_position.position.line as usize;
    let character = params.text_document_position.position.character as usize;

    let selected_line = vault.select_line(&path.to_path_buf(), line)?;



    let link_index = get_link_index(&selected_line, character);


    if let Some(index) = link_index {

        let range = Range {
            start: Position {
                line: line as u32,
                character: index as u32 + 1
            },
            end: Position {
                line: line as u32,
                character: character as u32
            }
        };

        let cmp_text = selected_line.get(index + 1 .. character)?;

        return match *cmp_text {
            [] => Some(CompletionResponse::List(CompletionList{
                items: initial_completion_files
                    .iter()
                    .filter_map(|path_i| {
                        Some(vault
                            .select_referenceable_nodes(Some(path_i))
                            .into_iter()
                            .filter(|referenceable| {
                                if initial_completion_files.len() > 1 {

                                    if *path_i != path {
                                        !matches!(referenceable, Referenceable::Tag(_, _))
                                        && !matches!(referenceable, Referenceable::Footnote(_, _))
                                    } else {
                                        false
                                    }

                                } else {

                                    !matches!(referenceable, Referenceable::Tag(_, _))
                                    && !matches!(referenceable, Referenceable::Footnote(_, _))

                                }
                            })
                            .collect_vec()
                        )})
                    .flatten()
                    .filter_map(|referenceable| completion_item(vault, &referenceable, None))
                    .collect::<Vec<_>>(),
                is_incomplete: true
            })),
            [' ', ref text @ ..] => {
                let blocks = vault.select_blocks();

                let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
                let mut matches = pattern::Pattern::parse(String::from_iter(text).as_str(), pattern::CaseMatching::Ignore, Normalization::Smart).match_list(blocks, &mut matcher);
                matches.par_sort_by_key(|(_, rank)| -(*rank as i32));


                let rand_id = nanoid!(5);





                return Some(CompletionResponse::List(CompletionList {
                    is_incomplete: true,
                    items: matches
                        .into_iter()
                        .take(50)
                        .filter(|(block, _)| String::from_iter(selected_line.clone()).trim() != block.text)
                        .filter_map(|(block, rank)| {
                            let path_ref = get_obsidian_ref_path(&vault.root_dir(), &block.file)?;
                            let url = Url::from_file_path(&block.file).ok()?;
                            Some(CompletionItem {
                                label: block.text.clone(),
                                sort_text: Some(rank.to_string()),
                                filter_text: Some(format!(" {}", block.text)),
                                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                                    range,
                                    new_text: format!("{}#^{}", path_ref, rand_id)
                                })),
                                data: Some(serde_json::to_value(BlockCompletionRequest {
                                    block: block.clone(),
                                    index: rand_id.clone()
                                }).ok()?),
                                command: Some(Command {
                                    title: "Insert Block Reference Into File".into(),
                                    command: "apply_edits".into(),
                                    arguments: Some(vec![
                                        serde_json::to_value(tower_lsp::lsp_types::WorkspaceEdit { 
                                            changes: Some(
                                                vec![( url, vec![
                                                    TextEdit {
                                                        range: Range {
                                                            start: Position {
                                                                line: block.range.end.line,
                                                                character: block.range.end.character - 1
                                                            },
                                                            end: Position {
                                                                line: block.range.end.line,
                                                                character: block.range.end.character - 1
                                                            }
                                                        },
                                                        new_text: format!("   ^{}", rand_id)
                                                    }
                                                ])]
                                                    .into_iter()
                                                    .collect()),
                                            change_annotations: None,
                                            document_changes: None
                                        }).ok()?
                                    ]),
                                }),
                                ..Default::default()
                            })
                        })
                        .collect()
                }))
            }
            [filter_char] => {

                let all_links = vault
                    .select_referenceable_nodes(None)
                    .into_par_iter()
                    .filter(|referenceable| {
                        matches!(referenceable, Referenceable::File(_, _))
                    });


                return Some(CompletionResponse::List(CompletionList{
                    is_incomplete: true,
                    items: all_links
                        .filter(|referenceable| referenceable.get_refname(&vault.root_dir()).map(|name| name.to_lowercase().starts_with(filter_char.to_ascii_lowercase())) == Some(true))
                        .filter_map(|referenceable| { completion_item(vault, &referenceable, Some(range)) })
                        .collect::<Vec<_>>(),

                }));

            },
            [filter_c1, filter_c2, ..] => {

                let all_links = vault
                    .select_referenceable_nodes(None)
                    .into_par_iter()
                    .filter(|referenceable| {
                        !matches!(referenceable, Referenceable::Tag(..))
                        && !matches!(referenceable, Referenceable::Footnote(..))
                    });

                return Some(CompletionResponse::List(CompletionList{
                    is_incomplete: false,
                    items: all_links
                        .filter(|referenceable| referenceable.get_refname(&vault.root_dir()).map(|name| name.to_lowercase().contains(filter_c1.to_ascii_lowercase())) == Some(true)
                            && referenceable.get_refname(&vault.root_dir()).map(|name| name.to_lowercase().contains(filter_c2.to_ascii_lowercase())) == Some(true)
                        )
                        .filter_map(|referenceable| {
                            completion_item(vault, &referenceable, Some(range))
                        })
                        .collect::<Vec<_>>(),
                }));
            }
        }

    } else if character
        .checked_sub(1)
        .and_then(|start| selected_line.get(start..character))
    == Some(&['#'])
    {
        // Initial Tag completion
        let tag_refereneables =
        vault
            .select_referenceable_nodes(None)
            .into_iter()
            .flat_map(|referenceable| match referenceable {
                tag @ Referenceable::Tag(_, _) => Some(tag),
                _ => None,
            });

        return Some(CompletionResponse::Array(
            tag_refereneables
                .filter_map(|tag| {
                    tag.get_refname(vault.root_dir())
                        .map(|root| CompletionItem {
                            kind: Some(CompletionItemKind::CONSTANT),
                            label: root,
                            ..Default::default()
                        })
                })
                .unique_by(|c| c.label.to_owned())
                .collect_vec(),
        ));
    } else if character
        .checked_sub(1)
        .and_then(|start| selected_line.get(start..character))
    == Some(&['['])
    {
        let footnote_referenceables = vault
            .select_referenceable_nodes(Some(&path))
            .into_iter()
            .flat_map(|referenceable| match referenceable {
                Referenceable::Footnote(footnote_path, _)
                    if footnote_path.as_path() == path.as_path() =>
                    {
                        Some(referenceable)
                    }
                _ => None,
            });

        return Some(CompletionResponse::Array(
            footnote_referenceables
                .filter_map(|footnote| {
                    footnote
                        .get_refname(vault.root_dir())
                        .map(|root| CompletionItem {
                            kind: Some(CompletionItemKind::REFERENCE),
                            label: root.clone(),
                            documentation: preview_referenceable(vault, &footnote)
                                .map(Documentation::MarkupContent),
                            filter_text: vault
                                .select_referenceable_preview(&footnote)
                                .and_then(|preview| match preview {
                                    Preview::Text(string) => Some(string),
                                    Preview::Empty => None,
                                })
                                .map(|preview_string| root + &preview_string),
                            ..Default::default()
                        })
                })
                .unique_by(|c| c.label.to_owned())
                .collect_vec(),
        ));
    } else {
        return None;
    }
}

fn calculate_hash<T: Hash>(t: &T) -> u64 {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    s.finish()
}

fn get_links(vault: &Vault) -> Option<Vec<Referenceable>> {
    let re = vault
        .select_referenceable_nodes(None)
        .into_iter()
        .filter(|referenceable| {
            !matches!(referenceable, Referenceable::Tag(_, _))
            && !matches!(referenceable, Referenceable::Footnote(_, _))
        })
        .collect_vec();

    Some(re)
}



fn completion_item(vault: &Vault, referenceable: &Referenceable, range: Option<Range>) -> Option<CompletionItem> {
    let refname = referenceable.get_refname(&vault.root_dir())?;
    let completion = CompletionItem {
        kind: Some(CompletionItemKind::FILE),
        label: refname.clone(),
        label_details: match referenceable.is_unresolved() {
            true => Some(CompletionItemLabelDetails {
                detail: Some("Unresolved".into()),
                description: None,
            }),
            false => None,
        },
        text_edit: range.map(|range| CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: refname.clone(),
        })),
        documentation: preview_referenceable(vault, &referenceable)
            .map(Documentation::MarkupContent),
        filter_text: match referenceable {
            Referenceable::IndexedBlock(_, _) => vault
                .select_referenceable_preview(&referenceable)
                .and_then(|preview| match preview {
                    Preview::Text(string) => Some(string),
                    Preview::Empty => None,
                })
                .map(|text| format!("{}{}", refname, &text)),
            _ => None,
        },
        ..Default::default()
    };

    Some(completion)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlockCompletionRequest {
    block: Block,
    index: String
}


pub async fn resolve_completion(completion: &CompletionItem, client: &Client)  -> Result<CompletionItem> {

    return Ok(completion.clone());

    if let Some(block_completion_request)  = completion.data.clone().and_then(|data| serde_json::from_value::<BlockCompletionRequest>(data).ok() ) {


        let Ok(url) = Url::from_file_path(&block_completion_request.block.file) else {
            return Ok(completion.clone())
        };

        let _ = client.apply_edit(tower_lsp::lsp_types::WorkspaceEdit { 
            changes: Some(
                vec![( url, vec![
                    TextEdit {
                        range: Range {
                            start: Position {
                                line: block_completion_request.block.range.end.line,
                                character: block_completion_request.block.range.end.character - 1
                            },
                            end: Position {
                                line: block_completion_request.block.range.end.line,
                                character: block_completion_request.block.range.end.character - 1
                            }
                        },
                        new_text: format!("   ^{}", block_completion_request.index)
                    }
                ])]
                    .into_iter()
                    .collect()),
            change_annotations: None,
            document_changes: None
        }).await;


        return Ok(completion.clone())


    } else {
        return Ok(completion.clone())
    }

}



#[cfg(test)]
mod tests {
    use super::get_link_index;

    #[test]
    fn test_index() {
        let s = "test [[linjfkdfjds]]";

        let expected = 6;

        let actual = get_link_index(&s.chars().collect(), 10);

        assert_eq!(Some(expected), actual);

        assert_eq!(Some("lin"), s.get(expected + 1 .. 10));
    }
}
