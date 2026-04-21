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
    pub fn label(&self, depth: usize) -> String {
        let indent = "  ".repeat(depth);
        match self {
            SchemaNode::Database { name, expanded, .. } => {
                format!("{}{}  {name}", indent, if *expanded { "▼" } else { "▶" })
            }
            SchemaNode::Table { name, expanded, .. } => {
                format!("{}{}  {name}", indent, if *expanded { "▼" } else { "▶" })
            }
            SchemaNode::Column {
                name,
                type_name,
                is_pk,
                ..
            } => {
                let pk = if *is_pk { " 🔑" } else { "" };
                format!("{indent}   {name}: {type_name}{pk}")
            }
        }
    }

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
    flatten_nodes(nodes, 0, &[], &mut items);
    items
}

fn flatten_nodes(
    nodes: &[SchemaNode],
    depth: usize,
    path: &[usize],
    items: &mut Vec<SchemaTreeItem>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let mut node_path = path.to_vec();
        node_path.push(i);
        items.push(SchemaTreeItem {
            label: node.label(depth),
            depth,
            node_path: node_path.clone(),
        });
        match node {
            SchemaNode::Database {
                expanded: true,
                tables,
                ..
            } => {
                flatten_nodes(tables, depth + 1, &node_path, items);
            }
            SchemaNode::Table {
                expanded: true,
                columns,
                ..
            } => {
                flatten_nodes(columns, depth + 1, &node_path, items);
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
