use std::collections::{BTreeMap, HashMap};

use egui::{self};
use egui_extras::TableBuilder;

use super::super::{ComfyJobInfo, ComfyJobStatus};
use crate::jobs_crate::{JobEvent, JobKind, JobStatus};
use crate::App;

#[derive(Clone, Debug)]
pub(crate) struct JobProgressSummary {
    pub id: String,
    pub label: String,
    pub value: f32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct JobStatusSummary {
    pub queued: usize,
    pub pending: usize,
    pub running: usize,
    pub progress: Option<JobProgressSummary>,
}

impl App {
    pub(crate) fn poll_jobs(&mut self) {
        let rx_opt = self.jobs.as_ref().map(|j| j.rx_events.clone());
        if let Some(rx) = rx_opt {
            while let Ok(ev) = rx.try_recv() {
                let status_str = match &ev.status {
                    JobStatus::Pending => "pending",
                    JobStatus::Running => "running",
                    JobStatus::Progress(_) => "progress",
                    JobStatus::Done => "done",
                    JobStatus::Failed(_) => "failed",
                    JobStatus::Canceled => "canceled",
                };
                let _ = self.db.update_job_status(&ev.id, status_str);

                self.job_events.push(ev);
                if self.job_events.len() > 300 {
                    self.job_events.remove(0);
                }
            }
        }

        if let Some(rx) = self.proxy_events.as_ref() {
            let mut drained = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                drained.push(ev);
            }
            for ev in drained {
                self.handle_proxy_event(ev);
            }
        }
    }

    pub(crate) fn job_status_summary(&self) -> JobStatusSummary {
        let mut latest: HashMap<String, JobEvent> = HashMap::new();
        for ev in &self.job_events {
            latest.insert(ev.id.clone(), ev.clone());
        }
        let mut summary = JobStatusSummary::default();
        for ev in latest.values() {
            match &ev.status {
                JobStatus::Pending => {
                    summary.pending += 1;
                    summary.queued += 1;
                }
                JobStatus::Running => {
                    summary.running += 1;
                    summary.queued += 1;
                }
                JobStatus::Progress(p) => {
                    summary.running += 1;
                    summary.queued += 1;
                    summary.progress.get_or_insert(JobProgressSummary {
                        id: ev.id.clone(),
                        label: format!("{:?}", ev.kind),
                        value: *p,
                    });
                }
                _ => {}
            }
        }
        if summary.progress.is_none() {
            for ev in self.job_events.iter().rev() {
                if let JobStatus::Progress(p) = ev.status {
                    if let Some(latest_status) = latest.get(&ev.id) {
                        if matches!(
                            latest_status.status,
                            JobStatus::Progress(_) | JobStatus::Running
                        ) {
                            summary.progress = Some(JobProgressSummary {
                                id: ev.id.clone(),
                                label: format!("{:?}", ev.kind),
                                value: p,
                            });
                            break;
                        }
                    }
                }
            }
        }
        summary.queued += self.comfy_queue_pending + self.comfy_queue_running;
        summary.pending += self.comfy_queue_pending;
        summary.running += self.comfy_queue_running;
        if summary.progress.is_none() {
            if let Some((pid, info)) = self
                .comfy_jobs
                .iter()
                .filter(|(_, info)| matches!(info.status, ComfyJobStatus::Running))
                .max_by_key(|(_, info)| info.updated_at)
            {
                summary.progress = Some(JobProgressSummary {
                    id: pid.clone(),
                    label: "ComfyUI".to_string(),
                    value: info.progress,
                });
            }
        }
        summary
    }

    pub(crate) fn jobs_window(&mut self, ctx: &egui::Context) {
        if !self.show_jobs {
            return;
        }

        egui::Window::new("Jobs")
            .open(&mut self.show_jobs)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label("Background Jobs");
                let mut latest: BTreeMap<String, JobEvent> = BTreeMap::new();
                for ev in &self.job_events {
                    latest.insert(ev.id.clone(), ev.clone());
                }
                TableBuilder::new(ui)
                    .striped(true)
                    .column(egui_extras::Column::auto())
                    .column(egui_extras::Column::auto())
                    .column(egui_extras::Column::auto())
                    .column(egui_extras::Column::remainder())
                    .header(18.0, |mut h| {
                        h.col(|ui| {
                            ui.strong("Job");
                        });
                        h.col(|ui| {
                            ui.strong("Asset");
                        });
                        h.col(|ui| {
                            ui.strong("Kind");
                        });
                        h.col(|ui| {
                            ui.strong("Status");
                        });
                    })
                    .body(|mut b| {
                        for (_id, ev) in latest.iter() {
                            b.row(20.0, |mut r| {
                                r.col(|ui| {
                                    ui.monospace(&ev.id[..8.min(ev.id.len())]);
                                });
                                r.col(|ui| {
                                    ui.monospace(&ev.asset_id[..8.min(ev.asset_id.len())]);
                                });
                                r.col(|ui| {
                                    ui.label(format!("{:?}", ev.kind));
                                });
                                r.col(|ui| {
                                    match &ev.status {
                                        JobStatus::Progress(p) => {
                                            ui.add(egui::ProgressBar::new(*p).show_percentage());
                                        }
                                        status => {
                                            ui.label(format!("{:?}", status));
                                        }
                                    }
                                    if !matches!(
                                        ev.status,
                                        JobStatus::Done
                                            | JobStatus::Failed(_)
                                            | JobStatus::Canceled
                                    ) {
                                        if ui.small_button("Cancel").clicked() {
                                            if let Some(j) = &self.jobs {
                                                j.cancel_job(&ev.id);
                                            }
                                        }
                                    }
                                });
                            });
                        }
                        let mut comfy_rows: Vec<(&String, &ComfyJobInfo)> =
                            self.comfy_jobs.iter().collect();
                        comfy_rows.sort_by_key(|(_, info)| info.updated_at);
                        for (pid, info) in comfy_rows {
                            let pid_fragment: String = pid.chars().take(8).collect::<String>();
                            b.row(20.0, |mut r| {
                                r.col(|ui| {
                                    ui.monospace(&pid_fragment);
                                });
                                r.col(|ui| {
                                    ui.label("ComfyUI");
                                });
                                r.col(|ui| {
                                    ui.label("ComfyUI");
                                });
                                r.col(|ui| match info.status {
                                    ComfyJobStatus::Queued => {
                                        ui.label("Queued");
                                    }
                                    ComfyJobStatus::Running => {
                                        ui.label("Running");
                                        ui.add(
                                            egui::ProgressBar::new(info.progress.clamp(0.0, 1.0))
                                                .desired_width(90.0)
                                                .show_percentage(),
                                        );
                                    }
                                    ComfyJobStatus::Completed => {
                                        ui.label("Completed");
                                    }
                                    ComfyJobStatus::Failed => {
                                        ui.label("Failed");
                                    }
                                });
                            });
                        }
                    });
            });
    }
}
