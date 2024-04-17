use std::{collections::HashMap, time::Duration};

use color_eyre::eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{prelude::*, widgets::*};
use serde::{Deserialize, Serialize};
use sn_service_management::{get_local_node_registry_path, NodeRegistry};
use tokio::sync::mpsc::UnboundedSender;

use super::{Component, Frame};
use crate::{
    action::Action,
    config::{Config, KeyBindings},
};

#[derive(Default)]
pub struct Home {
    command_tx: Option<UnboundedSender<Action>>,
    config: Config,
    node_registry: Option<NodeRegistry>,
}

impl Home {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Component for Home {
    fn register_action_handler(&mut self, tx: UnboundedSender<Action>) -> Result<()> {
        self.command_tx = Some(tx);
        Ok(())
    }

    fn register_config_handler(&mut self, config: Config) -> Result<()> {
        self.config = config;
        Ok(())
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        match action {
            Action::Tick => {
                let local_node_registry = NodeRegistry::load(&get_local_node_registry_path()?)?;

                if !local_node_registry.nodes.is_empty() {
                    self.node_registry = Some(local_node_registry);
                } else {
                    self.node_registry = None;
                }
            },
            _ => {},
        }
        Ok(None)
    }

    fn draw(&mut self, f: &mut Frame<'_>, area: Rect) -> Result<()> {
        if let Some(registry) = &self.node_registry {
            let nodes: Vec<_> =
                registry.to_status_summary().nodes.iter().filter_map(|n| n.peer_id.map(|p| p.to_string())).collect();
            let mut list = List::new(nodes);
            // table.add_row(Row::new(vec![Cell::new("Name"), Cell::new("Version"), Cell::new("Status")]));

            // for (name, node) in &nodes.nodes {
            //     table.add_row(Row::new(vec![Cell::new(name), Cell::new(&node.version), Cell::new(&node.status)]));
            // }

            f.render_widget(list, area);
        }
        f.render_widget(Paragraph::new("hello world"), area);
        Ok(())
    }
}
