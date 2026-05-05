//! Hardware provisioner — implements [`TuiProvisioner`] for in-TUI hardware/peripheral setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::{HardwareConfig, HardwareTransport};
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const HARDWARE_NAME: &str = "hardware";
pub const HARDWARE_DESC: &str =
    "Hardware peripherals — STM32, Raspberry Pi GPIO, serial/Probe transport";

#[derive(Debug, Clone)]
pub struct HardwareProvisioner;

impl HardwareProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HardwareProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for HardwareProvisioner {
    fn name(&self) -> &'static str {
        HARDWARE_NAME
    }

    fn description(&self) -> &'static str {
        HARDWARE_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Hardware
    }

    async fn run(&self, config: &mut Config, _profile: &Profile, io: ProvisionIo) -> Result<()> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Hardware peripherals enable physical world interaction via GPIO, serial, or JTAG/SWD Probe.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Choose {
                id: "enabled".into(),
                label: "Enable hardware peripherals?".into(),
                options: vec!["Disabled".to_string(), "Enabled".to_string()],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let enabled = sel.first().copied() == Some(1);

        if !enabled {
            config.hardware = HardwareConfig::default();
            send(
                &events,
                ProvisionEvent::Done {
                    summary: "Hardware peripherals disabled.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        send(
            &events,
            ProvisionEvent::Choose {
                id: "transport".into(),
                label: "Hardware transport".into(),
                options: vec![
                    "None".to_string(),
                    "Native (Linux/macOS GPIO)".to_string(),
                    "Serial (UART)".to_string(),
                    "Probe (JTAG/SWD)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let transport = match sel.first().copied() {
            Some(1) => HardwareTransport::Native,
            Some(2) => HardwareTransport::Serial,
            Some(3) => HardwareTransport::Probe,
            _ => HardwareTransport::None,
        };

        let serial_port: Option<String>;
        let probe_target: Option<String>;

        if transport == HardwareTransport::Serial {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "serial_port".into(),
                    label: "Serial port (e.g. /dev/ttyACM0)".into(),
                    default: Some("/dev/ttyACM0".into()),
                    secret: false,
                },
            )
            .await?;

            let port = recv_text(&mut responses).await?;
            serial_port = if port.trim().is_empty() {
                None
            } else {
                Some(port.trim().to_string())
            };
        } else {
            serial_port = None;
        }

        if transport == HardwareTransport::Probe {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "probe_target".into(),
                    label: "Probe target chip (e.g. STM32F401RE)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let target = recv_text(&mut responses).await?;
            probe_target = if target.trim().is_empty() {
                None
            } else {
                Some(target.trim().to_string())
            };
        } else {
            probe_target = None;
        }

        send(
            &events,
            ProvisionEvent::Choose {
                id: "datasheets".into(),
                label: "Enable workspace datasheet RAG (index PDF schematics for AI pin lookups)?"
                    .into(),
                options: vec!["No".to_string(), "Yes".to_string()],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let workspace_datasheets = sel.first().copied() == Some(1);

        let transport_for_summary = transport.clone();

        config.hardware = HardwareConfig {
            enabled: true,
            transport,
            serial_port,
            baud_rate: 115_200,
            probe_target,
            workspace_datasheets,
        };

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Hardware: {} transport enabled.", transport_for_summary),
            },
        )
        .await?;

        Ok(())
    }
}

use crate::onboard::provision::ProvisionerCategory;

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(ProvisionResponse::Text(t)) => Ok(t),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_hardware() {
        assert_eq!(HardwareProvisioner::new().name(), "hardware");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!HardwareProvisioner::new().description().is_empty());
    }
}
