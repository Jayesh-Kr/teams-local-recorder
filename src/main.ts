import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type RecordingStatus = "idle" | "recording" | "paused";
type RecordingSource = "auto_teams" | "manual";

interface RecordingState {
  status: RecordingStatus;
  source: RecordingSource | null;
  meeting_title: string;
  elapsed_seconds: number;
  is_mic_muted: boolean;
  is_teams_muted: boolean;
  is_manual_muted: boolean;
  participant_count: number;
  participant_names: string[];
  auto_record_enabled: boolean;
  in_teams_call: boolean;
  session_dir: string | null;
  last_error: string | null;
}

const pillEl = document.getElementById("pill") as HTMLDivElement;
const timerEl = document.getElementById("timer") as HTMLSpanElement;
const badgeEl = document.getElementById("participants-badge") as HTMLDivElement;
const countEl = document.getElementById("participant-count") as HTMLSpanElement;

const btnMute = document.getElementById("btn-mute") as HTMLButtonElement;
const iconMicOn = document.getElementById("icon-mic-on") as unknown as SVGSVGElement;
const iconMicOff = document.getElementById("icon-mic-off") as unknown as SVGSVGElement;

const btnPause = document.getElementById("btn-pause") as HTMLButtonElement;
const iconPause = document.getElementById("icon-pause") as unknown as SVGSVGElement;
const iconResume = document.getElementById("icon-resume") as unknown as SVGSVGElement;

const btnStop = document.getElementById("btn-stop") as HTMLButtonElement;

function formatDuration(totalSeconds: number): string {
  const safe = Math.max(0, Math.floor(totalSeconds));
  const hours = Math.floor(safe / 3600);
  const minutes = Math.floor((safe % 3600) / 60);
  const seconds = safe % 60;

  const mm = String(minutes).padStart(2, "0");
  const ss = String(seconds).padStart(2, "0");

  if (hours > 0) {
    return `${String(hours).padStart(2, "0")}:${mm}:${ss}`;
  }
  return `${mm}:${ss}`;
}

function renderState(state: RecordingState): void {
  pillEl.dataset.status = state.status;
  timerEl.textContent = formatDuration(state.elapsed_seconds);

  const titleLabel = state.meeting_title || "Recording";
  timerEl.title = titleLabel;

  // Participant badge & tooltip
  countEl.textContent = String(state.participant_count);
  if (state.participant_names.length > 0) {
    badgeEl.title = `${titleLabel}\nParticipants (${state.participant_count}):\n${state.participant_names.join("\n")}`;
  } else {
    badgeEl.title = `${titleLabel}\nNo Teams participants detected`;
  }

  // Mic mute state (Channel 0 muted while Channel 1 remote audio keeps recording)
  btnMute.classList.toggle("is-muted", state.is_mic_muted);
  iconMicOn.classList.toggle("hidden", state.is_mic_muted);
  iconMicOff.classList.toggle("hidden", !state.is_mic_muted);

  if (state.is_teams_muted && state.is_manual_muted) {
    btnMute.title = "Mic muted in Teams & recorder (remote audio still recording)";
  } else if (state.is_teams_muted) {
    btnMute.title = "Mic muted in Teams (remote audio still recording)";
  } else if (state.is_manual_muted) {
    btnMute.title = "Local mic muted — click to unmute (remote audio still recording)";
  } else {
    btnMute.title = "Mute local mic (remote audio continues recording)";
  }

  // Pause / Resume state
  const isPaused = state.status === "paused";
  btnPause.classList.toggle("is-paused", isPaused);
  iconPause.classList.toggle("hidden", isPaused);
  iconResume.classList.toggle("hidden", !isPaused);
  btnPause.title = isPaused ? "Resume recording" : "Pause recording";
}

btnMute.addEventListener("click", async () => {
  try {
    const next = await invoke<RecordingState>("toggle_local_mute");
    renderState(next);
  } catch (err) {
    console.error("toggle_local_mute failed:", err);
  }
});

btnPause.addEventListener("click", async () => {
  try {
    const next = await invoke<RecordingState>("toggle_pause");
    renderState(next);
  } catch (err) {
    console.error("toggle_pause failed:", err);
  }
});

btnStop.addEventListener("click", async () => {
  try {
    const next = await invoke<RecordingState>("stop_recording");
    renderState(next);
  } catch (err) {
    console.error("stop_recording failed:", err);
  }
});

async function bootstrap(): Promise<void> {
  await listen<RecordingState>("recording://state-updated", (event) => {
    renderState(event.payload);
  });

  try {
    const initial = await invoke<RecordingState>("get_recording_state");
    renderState(initial);
  } catch (err) {
    console.error("get_recording_state failed:", err);
  }
}

void bootstrap();
