/// Schema tree node for the schema browser panel.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SchemaNode {
    Database {
        name: String,
        expanded: bool,
        tables: Vec<SchemaNode>,
    },
    Table {
        name: String,
        expanded: bool,
        columns: Vec<SchemaNode>,
    },
    Column {
        name: String,
        type_name: String,
        nullable: bool,
        is_pk: bool,
    },
}

impl SchemaNode {
    pub fn name(&self) -> &str {
        match self {
            SchemaNode::Database { name, .. } => name,
            SchemaNode::Table { name, .. } => name,
            SchemaNode::Column { name, .. } => name,
        }
    }

    pub fn is_expanded(&self) -> bool {
        match self {
            SchemaNode::Database { expanded, .. } => *expanded,
            SchemaNode::Table { expanded, .. } => *expanded,
            SchemaNode::Column { .. } => false,
        }
    }

    pub fn toggle(&mut self) {
        match self {
            SchemaNode::Database { expanded, .. } => *expanded = !*expanded,
            SchemaNode::Table { expanded, .. } => *expanded = !*expanded,
            SchemaNode::Column { .. } => {}
        }
    }
}

/// Flat list of visible tree items for rendering.
#[derive(Debug, Clone)]
pub struct SchemaTreeItem {
    pub label: String,
    pub depth: usize,
    pub node_path: Vec<usize>, // indices to reach this node from root
}

pub fn flatten_tree(nodes: &[SchemaNode]) -> Vec<SchemaTreeItem> {
    let mut items = Vec::new();
    flatten_nodes(nodes, 0, &[], &[], &mut items);
    items
}

/// Flatten ALL nodes regardless of expanded state, using simple depth indentation.
/// Used for search so collapsed subtrees are still searchable.
pub fn flatten_all(nodes: &[SchemaNode]) -> Vec<SchemaTreeItem> {
    let mut items = Vec::new();
    flatten_nodes_all(nodes, 0, &[], &mut items);
    items
}

fn flatten_nodes_all(
    nodes: &[SchemaNode],
    depth: usize,
    path: &[usize],
    items: &mut Vec<SchemaTreeItem>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let mut node_path = path.to_vec();
        node_path.push(i);
        let indent = " ".repeat(1 + depth * 2);
        let icon = node_icon(node);
        let name = node.name();
        let extra = match node {
            SchemaNode::Column { type_name, .. } => format!(": {type_name}"),
            _ => String::new(),
        };
        let label = format!("{indent}{icon}{name}{extra}");
        items.push(SchemaTreeItem {
            label,
            depth,
            node_path: node_path.clone(),
        });
        match node {
            SchemaNode::Database { tables, .. } => {
                flatten_nodes_all(tables, depth + 1, &node_path, items);
            }
            SchemaNode::Table { columns, .. } => {
                flatten_nodes_all(columns, depth + 1, &node_path, items);
            }
            _ => {}
        }
    }
}

fn node_icon(node: &SchemaNode) -> &'static str {
    match node {
        SchemaNode::Database { .. } => "󰆼 ",
        SchemaNode::Table { .. } => "󰓫 ",
        SchemaNode::Column { is_pk: true, .. } => "󰌆 ",
        SchemaNode::Column { .. } => "󱘚 ",
    }
}

fn flatten_nodes(
    nodes: &[SchemaNode],
    depth: usize,
    path: &[usize],
    _ancestor_is_last: &[bool],
    items: &mut Vec<SchemaTreeItem>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let mut node_path = path.to_vec();
        node_path.push(i);

        let indent = " ".repeat(1 + depth * 2);
        let icon = node_icon(node);
        let name = node.name();
        let extra = match node {
            SchemaNode::Column { type_name, .. } => format!(": {type_name}"),
            _ => String::new(),
        };
        let label = format!("{indent}{icon}{name}{extra}");

        items.push(SchemaTreeItem {
            label,
            depth,
            node_path: node_path.clone(),
        });

        let child_ancestor_is_last: Vec<bool> = Vec::new();

        match node {
            SchemaNode::Database {
                expanded: true,
                tables,
                ..
            } => {
                flatten_nodes(
                    tables,
                    depth + 1,
                    &node_path,
                    &child_ancestor_is_last,
                    items,
                );
            }
            SchemaNode::Table {
                expanded: true,
                columns,
                ..
            } => {
                flatten_nodes(
                    columns,
                    depth + 1,
                    &node_path,
                    &child_ancestor_is_last,
                    items,
                );
            }
            _ => {}
        }
    }
}

/// Copy `expanded` flags from `old` into `new` by matching node names at each level.
/// Called before replacing schema nodes on a background refresh so the user's
/// open/closed state is preserved.
pub fn merge_expansion(old: &[SchemaNode], new: &mut [SchemaNode]) {
    for new_node in new.iter_mut() {
        let Some(old_node) = old.iter().find(|o| o.name() == new_node.name()) else {
            continue;
        };
        match (old_node, new_node) {
            (
                SchemaNode::Database {
                    expanded: old_exp,
                    tables: old_tables,
                    ..
                },
                SchemaNode::Database {
                    expanded: new_exp,
                    tables: new_tables,
                    ..
                },
            ) => {
                *new_exp = *old_exp;
                merge_expansion(old_tables, new_tables);
            }
            (
                SchemaNode::Table {
                    expanded: old_exp,
                    columns: old_cols,
                    ..
                },
                SchemaNode::Table {
                    expanded: new_exp,
                    columns: new_cols,
                    ..
                },
            ) => {
                *new_exp = *old_exp;
                merge_expansion(old_cols, new_cols);
            }
            _ => {}
        }
    }
}

/// Walk `node_path` indices through the tree and return the joined name string, e.g. `"mydb/users/id"`.
pub fn path_to_string(path: &[usize], nodes: &[SchemaNode]) -> String {
    let mut parts = Vec::new();
    let mut current = nodes;
    for &idx in path {
        let Some(node) = current.get(idx) else {
            break;
        };
        parts.push(node.name().to_string());
        match node {
            SchemaNode::Database { tables, .. } => current = tables,
            SchemaNode::Table { columns, .. } => current = columns,
            SchemaNode::Column { .. } => break,
        }
    }
    parts.join("/")
}

/// Find the flat-list index of the visible item whose tree path matches `path_str`.
pub fn find_cursor_by_path(
    items: &[SchemaTreeItem],
    nodes: &[SchemaNode],
    path_str: &str,
) -> Option<usize> {
    items
        .iter()
        .position(|item| path_to_string(&item.node_path, nodes) == path_str)
}

/// Expand all ancestor nodes needed so the item at `path_str` becomes visible.
/// E.g. for `"mydb/users/id"` this expands the `mydb` database and the `users` table.
pub fn expand_path(nodes: &mut Vec<SchemaNode>, path_str: &str) {
    let parts: Vec<&str> = path_str.splitn(3, '/').collect();
    // Need to expand: Database for parts[0] (when parts.len() >= 2),
    // and Table for parts[1] inside that db (when parts.len() >= 3).
    if parts.len() < 2 {
        return;
    }
    for node in nodes.iter_mut() {
        if let SchemaNode::Database {
            name,
            expanded,
            tables,
        } = node
            && name == parts[0] {
                *expanded = true;
                if parts.len() >= 3 {
                    for table in tables.iter_mut() {
                        if let SchemaNode::Table {
                            name: tname,
                            expanded: texpanded,
                            ..
                        } = table
                            && tname == parts[1] {
                                *texpanded = true;
                                break;
                            }
                    }
                }
                break;
            }
    }
}

/// Collect path strings for every expanded Database/Table node, e.g. `["mydb", "mydb/users"]`.
pub fn collect_expanded_paths(nodes: &[SchemaNode]) -> Vec<String> {
    let mut paths = Vec::new();
    for node in nodes {
        if let SchemaNode::Database {
            name,
            expanded: true,
            tables,
        } = node
        {
            paths.push(name.clone());
            for table in tables {
                if let SchemaNode::Table {
                    name: tname,
                    expanded: true,
                    ..
                } = table
                {
                    paths.push(format!("{name}/{tname}"));
                }
            }
        }
    }
    paths
}

/// Expand the nodes named by each path string (inverse of `collect_expanded_paths`).
pub fn restore_expanded_paths(nodes: &mut Vec<SchemaNode>, paths: &[String]) {
    for path in paths {
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        match parts.as_slice() {
            [db_name] => {
                for node in nodes.iter_mut() {
                    if let SchemaNode::Database { name, expanded, .. } = node
                        && name == db_name
                    {
                        *expanded = true;
                        break;
                    }
                }
            }
            [db_name, table_name] => {
                for node in nodes.iter_mut() {
                    if let SchemaNode::Database { name, tables, .. } = node
                        && name == db_name
                    {
                        for table in tables.iter_mut() {
                            if let SchemaNode::Table {
                                name: tname,
                                expanded,
                                ..
                            } = table
                                && tname == table_name
                            {
                                *expanded = true;
                                break;
                            }
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn toggle_node(nodes: &mut [SchemaNode], path: &[usize]) {
    if path.is_empty() {
        return;
    }
    let idx = path[0];
    if idx >= nodes.len() {
        return;
    }
    if path.len() == 1 {
        nodes[idx].toggle();
        return;
    }
    match &mut nodes[idx] {
        SchemaNode::Database { tables, .. } => toggle_node(tables, &path[1..]),
        SchemaNode::Table { columns, .. } => toggle_node(columns, &path[1..]),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> Vec<SchemaNode> {
        vec![SchemaNode::Database {
            name: "mydb".into(),
            expanded: false,
            tables: vec![SchemaNode::Table {
                name: "users".into(),
                expanded: false,
                columns: vec![SchemaNode::Column {
                    name: "id".into(),
                    type_name: "INT".into(),
                    nullable: false,
                    is_pk: true,
                }],
            }],
        }]
    }

    #[test]
    fn flatten_collapsed_shows_only_databases() {
        let tree = sample_tree();
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 1);
        assert!(items[0].label.contains("mydb"));
    }

    #[test]
    fn expand_database_shows_tables() {
        let mut tree = sample_tree();
        toggle_node(&mut tree, &[0]);
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 2);
        assert!(items[1].label.contains("users"));
    }

    #[test]
    fn expand_table_shows_columns() {
        let mut tree = sample_tree();
        toggle_node(&mut tree, &[0]); // expand db
        toggle_node(&mut tree, &[0, 0]); // expand table
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 3);
        assert!(items[2].label.contains("id"));
    }

    #[test]
    fn collapse_database_hides_children() {
        let mut tree = sample_tree();
        toggle_node(&mut tree, &[0]); // expand
        toggle_node(&mut tree, &[0]); // collapse
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn cursor_bounds_respected() {
        let tree = sample_tree();
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 1);
        // Cursor cannot go below 0 or above items.len()-1
        let cursor: usize = 0;
        let next = cursor.saturating_add(1).min(items.len().saturating_sub(1));
        assert_eq!(next, 0); // only 1 item, stays at 0
    }
}
