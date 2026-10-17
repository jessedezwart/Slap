const { invoke } = window.__TAURI__.core;

const statusEl = document.getElementById("status");
const errorEl = document.getElementById("error");
const countEl = document.getElementById("count");
const guildsEl = document.getElementById("guilds");

function cell(text, className) {
  const td = document.createElement("td");
  td.textContent = text;
  if (className) td.className = className;
  return td;
}

function renderGuilds(guilds) {
  countEl.textContent = guilds.length ? `(${guilds.length})` : "";

  if (!guilds.length) {
    guildsEl.replaceChildren();
    const tr = document.createElement("tr");
    tr.append(cell("No guilds yet.", "muted"));
    tr.firstChild.colSpan = 3;
    guildsEl.append(tr);
  } else {
    guildsEl.replaceChildren(...guilds.map((guild) => {
      const tr = document.createElement("tr");
      tr.title = guild.id;
      const voice = document.createElement("td");
      const dot = document.createElement("span");
      dot.className = guild.in_voice ? "dot on" : "dot";
      voice.append(dot, guild.in_voice ? " playing" : "");
      tr.append(cell(guild.name), cell(guild.member_count ?? "—", "num"), voice);
      return tr;
    }));
  }
}

async function refresh() {
  try {
    const snapshot = await invoke("snapshot");
    statusEl.textContent = snapshot.status;
    statusEl.className = `pill ${snapshot.status}`;
    errorEl.hidden = !snapshot.error;
    errorEl.textContent = snapshot.error ?? "";
    renderGuilds(snapshot.guilds);
  } catch (error) {
    errorEl.hidden = false;
    errorEl.textContent = String(error);
  }
}

refresh();
setInterval(refresh, 2000);
