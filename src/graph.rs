//! Read-only queries over a Graphify knowledge graph (`<project>/graphify-out/graph.json`).
//!
//! Graphify builds the graph; the broker only reads it. Graphs are parsed once, kept until the
//! file changes, and dropped with the other idle resources.

use crate::config::GRAPHIFY_OUT;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_CANDIDATES: usize = 10;

#[derive(Deserialize)]
struct RawGraph {
    #[serde(default)]
    nodes: Vec<RawNode>,
    #[serde(default, alias = "edges")]
    links: Vec<RawLink>,
}

#[derive(Deserialize)]
struct RawNode {
    id: Value,
    #[serde(default)]
    label: Value,
    #[serde(default)]
    source_file: Value,
    #[serde(default)]
    source_location: Value,
    #[serde(default)]
    community: Value,
    #[serde(default)]
    community_name: Value,
}

#[derive(Deserialize)]
struct RawLink {
    source: Value,
    target: Value,
    #[serde(default)]
    relation: Value,
    #[serde(default)]
    confidence: Value,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

struct Node {
    id: String,
    label: String,
    lowered: String,
    /// `file location`, as Graphify recorded it.
    place: String,
    community: String,
    edges: Vec<usize>,
}

struct Edge {
    source: usize,
    target: usize,
    relation: String,
    /// Empty for edges read directly from the source (`EXTRACTED`); otherwise Graphify's tag.
    confidence: String,
}

pub struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

pub fn graph_file(root: &Path) -> PathBuf {
    root.join(GRAPHIFY_OUT).join("graph.json")
}

/// Parsed graphs by project root, each with the modification time it was read at.
#[derive(Default)]
pub struct Graphs(HashMap<PathBuf, (SystemTime, Graph)>);

impl Graphs {
    pub fn release(&mut self) -> bool {
        let had = !self.0.is_empty();
        self.0.clear();
        had
    }

    pub fn get(&mut self, root: &Path) -> Result<&Graph, String> {
        let path = graph_file(root);
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .map_err(|_| format!("no Graphify graph at {}; build one with `graphify extract .`", path.display()))?;
        if self.0.get(root).is_none_or(|(seen, _)| *seen != modified) {
            let file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let raw: RawGraph = serde_json::from_reader(std::io::BufReader::new(file))
                .map_err(|e| format!("unreadable Graphify graph {}: {e}", path.display()))?;
            self.0.insert(root.to_path_buf(), (modified, Graph::build(raw)));
        }
        Ok(&self.0[root].1)
    }
}

impl Graph {
    fn build(raw: RawGraph) -> Self {
        let mut nodes: Vec<Node> = raw
            .nodes
            .iter()
            .map(|node| {
                let id = text(&node.id);
                let label = Some(text(&node.label)).filter(|label| !label.is_empty()).unwrap_or_else(|| id.clone());
                let community = Some(text(&node.community_name)).filter(|name| !name.is_empty()).unwrap_or_else(|| text(&node.community));
                Node {
                    lowered: label.to_lowercase(),
                    place: format!("{} {}", text(&node.source_file), text(&node.source_location)).trim().to_string(),
                    edges: Vec::new(),
                    id,
                    label,
                    community,
                }
            })
            .collect();
        let by_id: HashMap<&str, usize> = nodes.iter().enumerate().map(|(index, node)| (node.id.as_str(), index)).collect();
        // Edges whose endpoints are not in the node set are dropped, as Graphify does.
        let edges: Vec<Edge> = raw
            .links
            .iter()
            .filter_map(|link| {
                let confidence = text(&link.confidence);
                Some(Edge {
                    source: *by_id.get(text(&link.source).as_str())?,
                    target: *by_id.get(text(&link.target).as_str())?,
                    relation: text(&link.relation),
                    confidence: if confidence == "EXTRACTED" { String::new() } else { confidence },
                })
            })
            .collect();
        for (index, edge) in edges.iter().enumerate() {
            nodes[edge.source].edges.push(index);
            if edge.target != edge.source {
                nodes[edge.target].edges.push(index);
            }
        }
        Self { nodes, edges }
    }

    fn describe(&self, index: usize) -> String {
        let node = &self.nodes[index];
        if node.place.is_empty() { node.label.clone() } else { format!("{} · {}", node.label, node.place) }
    }

    /// The node `name` refers to, or the reply to send when it is unknown or ambiguous.
    fn resolve(&self, name: &str) -> Result<usize, String> {
        let wanted = name.trim();
        let lowered = wanted.to_lowercase();
        if lowered.is_empty() {
            return Err("node is required".into());
        }
        let busiest = |candidates: &[usize]| candidates.iter().copied().max_by_key(|&index| self.nodes[index].edges.len());
        let matching = |test: &dyn Fn(&Node) -> bool| -> Vec<usize> { (0..self.nodes.len()).filter(|&index| test(&self.nodes[index])).collect() };
        if let Some(index) = self.nodes.iter().position(|node| node.id == wanted) {
            return Ok(index);
        }
        // Same label on several nodes (a method name, say): take the most connected one.
        if let Some(index) = busiest(&matching(&|node| node.lowered == lowered)) {
            return Ok(index);
        }
        let mut candidates = matching(&|node| node.lowered.contains(&lowered));
        if candidates.is_empty() {
            // A question rather than a name: rank nodes by how many of its words they carry.
            let words: Vec<&str> = lowered.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|word| word.len() > 2).collect();
            let score = |node: &Node| words.iter().filter(|word| node.lowered.contains(**word)).count();
            candidates = matching(&|node| score(node) > 0);
            candidates.sort_by_key(|&index| std::cmp::Reverse(score(&self.nodes[index])));
        }
        match candidates.len() {
            0 => Err(format!("No node matches '{wanted}'.")),
            1 => Ok(candidates[0]),
            total => {
                // A stable sort: for word matches the best-scoring nodes stay ahead of busier ones.
                if candidates.iter().all(|&index| self.nodes[index].lowered.contains(&lowered)) {
                    candidates.sort_by_key(|&index| std::cmp::Reverse(self.nodes[index].edges.len()));
                }
                let mut out = format!("{total} nodes match '{wanted}'; pass one id as node:");
                for &index in candidates.iter().take(MAX_CANDIDATES) {
                    out.push_str(&format!("\n{} — {}", self.nodes[index].id, self.describe(index)));
                }
                Err(out)
            }
        }
    }

    /// A node with its connections, most connected neighbours first.
    pub fn explain(&self, name: &str, limit: usize) -> String {
        let index = match self.resolve(name) {
            Ok(index) => index,
            Err(reply) => return reply,
        };
        let node = &self.nodes[index];
        let mut out = self.describe(index);
        if !node.community.is_empty() {
            out.push_str(&format!(" · community {}", node.community));
        }
        out.push_str(&format!(" · {} edges", node.edges.len()));
        let mut links: Vec<(usize, &Edge)> = node
            .edges
            .iter()
            .map(|&edge| &self.edges[edge])
            .map(|edge| (if edge.source == index { edge.target } else { edge.source }, edge))
            .collect();
        links.sort_by_key(|(other, _)| std::cmp::Reverse(self.nodes[*other].edges.len()));
        let shown = limit.clamp(1, 200);
        for (other, edge) in links.iter().take(shown) {
            let arrow = if edge.source == index { "-->" } else { "<--" };
            let tag = if edge.confidence.is_empty() { edge.relation.clone() } else { format!("{}, {}", edge.relation, edge.confidence) };
            out.push_str(&format!("\n{arrow} {} [{tag}]", self.describe(*other)));
        }
        if links.len() > shown {
            out.push_str(&format!("\n+{} more (raise limit)", links.len() - shown));
        }
        out
    }

    /// The shortest chain of edges between two nodes, ignoring edge direction.
    pub fn path(&self, from: &str, to: &str) -> String {
        let (start, goal) = match (self.resolve(from), self.resolve(to)) {
            (Ok(start), Ok(goal)) => (start, goal),
            (Err(reply), _) | (_, Err(reply)) => return reply,
        };
        // Breadth-first search; `came_from[n]` is the (node, edge) that first reached `n`.
        let mut came_from: Vec<Option<(usize, usize)>> = vec![None; self.nodes.len()];
        let mut queue = VecDeque::from([start]);
        while let Some(current) = queue.pop_front() {
            if current == goal {
                break;
            }
            for &edge in &self.nodes[current].edges {
                let next = if self.edges[edge].source == current { self.edges[edge].target } else { self.edges[edge].source };
                if next != start && came_from[next].is_none() {
                    came_from[next] = Some((current, edge));
                    queue.push_back(next);
                }
            }
        }
        if start != goal && came_from[goal].is_none() {
            return format!("No path between {} and {}.", self.nodes[start].label, self.nodes[goal].label);
        }
        let mut steps = Vec::new();
        let mut current = goal;
        while let Some((previous, edge)) = came_from[current] {
            let link = &self.edges[edge];
            let arrow = if link.source == previous { format!("--{}-->", link.relation) } else { format!("<--{}--", link.relation) };
            steps.push(format!(" {arrow} {}", self.nodes[current].label));
            current = previous;
        }
        steps.reverse();
        format!("{} hop(s): {}{}", steps.len(), self.nodes[start].label, steps.concat())
    }
}
