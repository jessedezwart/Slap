# Architecture

## Modules

```mermaid
flowchart TB
    main["main<br/>runtime, Ctrl-C"] --> bot["bot<br/>startup, shutdown, BotContext"]
    main --> config["config"]

    bot --> discord["discord<br/>gateway, interactions, player message"]
    discord --> bot
    bot --> session["session<br/>guild sessions, queue, idle, events"]
    bot --> audio["audio<br/>TrackSource, HttpStream, RangedDownload"]
    bot --> media["media<br/>importer, resolver, search cache"]

    discord --> session
    discord --> audio
    discord --> media
    session --> media
    audio --> media
    media --> config

    discord -.-> twilight(["twilight"])
    session -.-> songbird(["songbird"])
    audio -.-> songbird
    media -.-> ytdlp(["yt-dlp process"])
    media -.-> spotify(["Spotify API"])
    media -.-> redb(["redb file"])
    session --> history["history<br/>all-time play stats"]
    discord --> history
    history -.-> statsdb(["redb file"])
    audio -.-> host(["Audio host"])
```

## What lives how long

```mermaid
flowchart TB
    subgraph process["Process: main to Ctrl-C"]
        bot["Bot"]
        ctx["BotContext (Arc)<br/>http, cache, songbird, importer, audio"]
        manager["SessionManager<br/>guild slots, closed flag"]
        shutdown(("shutdown token"))
        bot --> ctx --> manager

        subgraph shardlife["One task per shard"]
            shard["run_shard"]
        end

        subgraph interactionlife["One task per interaction: untracked"]
            interaction["handle_interaction"]
        end

        subgraph guildlife["One per guild, until teardown"]
            session["GuildSession<br/>channel, redraw, player message"]
            cancel(("session cancel token"))
            voice["songbird Call<br/>track queue, global events"]
            idle["leave_when_idle task"]
            player["PlayerTask<br/>tracked by player_tasks"]

            subgraph tracklife["One per queued track"]
                source["TrackSource: lazy until played or preloaded"]
                stream["HttpStream and RangedDownload"]
                source --> stream
            end

            session --> cancel
            voice --> source
        end

        manager --> session
    end

    shutdown -.-> shard
    cancel -.-> idle
    cancel -.-> player
    cancel -.-> source
    cancel -.-> stream
    cancel -.-> interaction
```

## Session lifecycle

```mermaid
stateDiagram-v2
    [*] --> NoSession
    NoSession --> NoSession: join refused, manager closed
    NoSession --> Joining: join
    Joining --> Active: songbird join ok
    Joining --> NoSession: join failed
    Active --> Active: join from same channel reuses session
    Active --> NoSession: teardown_locked

    state Active {
        [*] --> Unclaimed
        Unclaimed --> Claimed: claim_player_component
        Claimed --> Unclaimed: player message gone (403 or 404)
    }

    NoSession --> [*]: process exit
```

### What teardown does

```mermaid
flowchart LR
    take["Take the session<br/>out of the slot"] --> cancel["Fire the<br/>cancellation token"]
    cancel --> queue["Stop the queue and<br/>remove global events"]
    queue --> remove["songbird.remove<br/>leaves voice"]

    cancel -.-> idle["leave_when_idle<br/>returns"]
    cancel -.-> player["PlayerTask edits the message<br/>to stopped, or deletes it"]
    cancel -.-> download["Downloads stop,<br/>tracks are dropped"]
    cancel -.-> import["A running /play import<br/>fails with Ended"]
```

## Where events come in

There are two entry points: the Discord gateway, once per shard, and songbird's
per-call events.

```mermaid
flowchart LR
    discord(["Discord gateway"]) --> shard["run_shard"]
    shard --> cache["discord_cache.update<br/>voice states only"]
    cache --> process["songbird.process<br/>finishes voice connections"]
    process --> handle["handle_event"]

    handle -->|InteractionCreate| spawn["spawn<br/>handle_interaction"]
    handle -->|VoiceStateUpdate| reconcile["reconcile_voice"]
    handle -->|GatewayClose| log["log a warning"]

    spawn --> read["Request::read"]
    read -->|"/play with input<br/>modal submit"| play["play"]
    read -->|"/play without input<br/>Add button"| modal["open_modal"]
    read -->|"Pause, Resume,<br/>Skip, Stop"| control["SessionManager::control"]
    read -->|"Stats button"| stats["PlayHistory::summary"]
    reconcile --> teardownif["teardown_if"]

    driver(["songbird driver"]) -->|DriverDisconnect| disconnect["DisconnectHandler"]
    driver -->|"Play, Pause, End,<br/>Error, Playable"| track["TrackEventHandler"]
    disconnect --> teardownif
    track --> redraw["redraw.notify_one"]
    redraw --> ptask["PlayerTask redraws"]
```


## Sequence diagram of /play

```mermaid
sequenceDiagram
    actor User
    participant Discord
    participant Task as handle_interaction
    participant Sessions as SessionManager
    participant Importer
    participant Player as PlayerTask

    User->>Discord: /play query
    Discord->>Task: InteractionCreate
    Task->>Discord: defer
    Task->>Sessions: join
    alt no session
        Sessions->>Sessions: join voice, spawn leave_when_idle
        Sessions-->>Task: new token
    else session in this channel
        Sessions-->>Task: existing token
    else session in another channel, or shutting down
        Sessions-->>Task: WrongChannel or Closed
        Task->>Discord: update reply with the error
    end
    Note over Task: the rest only runs with a token
    Task->>Task: reconcile_voice, in case the caller just left
    Task->>Importer: import
    Importer-->>Task: tracks
    Task->>Sessions: enqueue
    Note over Task,Sessions: songbird takes over from here, see Queue to voice
    Task->>Discord: update reply: Queued N tracks
    Task->>Sessions: claim_player_component
    alt no player on this session yet
        Sessions-->>Task: session
        Task->>Player: spawn on player_tasks
        loop until the session ends or the message is gone
            Player->>Discord: edit message
            Player->>Player: wait for redraw or cancel
        end
    else session already has a player
        Sessions-->>Task: None, reply stays as text
    else session ended meanwhile
        Task->>Discord: delete the reply
    end
```

## Import and resolve

```mermaid
sequenceDiagram
    participant Task as handle_interaction
    participant Importer
    participant Queue as TrackQueue
    participant Source as TrackSource
    participant Resolver as YtDlpResolver

    Note over Task,Queue: During /play
    Task->>Importer: import
    Importer-->>Task: ImportedTracks
    Task->>Task: shuffle, wrap each in a TrackSource
    Task->>Queue: enqueue

    Note over Queue,Resolver: Later, once for each track that gets opened
    Queue->>Source: create_async
    alt has a stream that lasts at least 2 more minutes
        Source->>Source: reuse the stream
    else no stream, or it is about to expire
        Source->>Resolver: resolve target
        Resolver-->>Source: StreamInfo
        Source->>Source: keep it, a Search target becomes the found URL
    end
    Source-->>Queue: audio stream, or the track errors
```

### How a target resolves

```mermaid
flowchart TD
    target{"Target"}
    target -->|Url| direct["yt-dlp -j"]
    target -->|Search| cached{"Upload in<br/>SearchCache?"}

    direct -->|ok| done(["StreamInfo"])
    direct -->|failed| failed(["Track errors"])

    cached -->|yes| recheck["yt-dlp -j<br/>on the cached upload"]
    cached -->|no| source["Next source:<br/>YouTube Music, then YouTube"]
    recheck -->|ok| done
    recheck -->|failed, not fatal| source
    recheck -->|fatal| failed

    source --> search["yt-dlp -j<br/>on the top result"]
    search -->|ok| duration{"Within 7% or 10s,<br/>or a duration unknown?"}
    search -->|failed, not fatal| more{"Sources left?"}
    search -->|fatal| failed

    duration -->|yes| remember["Save upload<br/>to SearchCache"]
    duration -->|no| more
    remember --> done
    more -->|yes| source
    more -->|no| failed
```

## Queue to voice

songbird owns the queue. `enqueue` hands it every track straight away, paused
and unopened, so a long playlist costs no downloads. The tracks advance through
events that songbird attaches to each track. slap never starts the next track
itself: Skip only stops the current one, which fires the same `End` event.

```mermaid
sequenceDiagram
    participant Sessions as SessionManager
    participant Queue as TrackQueue
    participant Mixer as songbird mixer
    participant Source as TrackSource
    participant Player as PlayerTask

    Sessions->>Queue: enqueue_with_preload, duration minus 10s
    Queue->>Mixer: add track, paused
    opt queue was empty
        Queue->>Mixer: play the head
        Mixer->>Source: create_async
        Source-->>Mixer: audio stream
    end
    Sessions-->>Player: redraw
    Mixer-->>Player: Play, Playable events cause a redraw

    Note over Queue,Mixer: Head reaches its duration minus 10s
    Mixer-->>Queue: SongPreloader fires
    Queue->>Mixer: make_playable on the second track
    Mixer->>Source: create_async
    Source-->>Mixer: audio stream, track stays paused

    Note over Queue,Mixer: Head finishes, is skipped, or errors
    Mixer-->>Queue: QueueHandler fires on End
    Queue->>Queue: pop the head
    loop until a track accepts play, or the queue is empty
        Queue->>Mixer: play the new head
    end
    Mixer-->>Player: End, Play events cause a redraw
```

## Audio pipeline

Reading is pull-based. The mixer drains the ring buffer, and when that buffer is
full, nothing polls the `HttpStream`, so no new window is requested. A preloaded
track that hasn't started waits this way, holding roughly one budget of audio.

```mermaid
flowchart TB
    host(["Audio host"])

    subgraph async["tokio tasks"]
        download["RangedDownload<br/>256 KiB range requests, one after another"]
        stream["HttpStream<br/>prefill, then the rest of the download"]
        adapter["AsyncAdapterStream sink<br/>copies bytes across"]
    end

    ring[("Ring buffer<br/>budget_bytes")]

    subgraph driver["songbird driver"]
        demux["symphonia<br/>reads Opus packets"]
        mixer["Mixer<br/>passes Opus through, or decodes and re-encodes"]
    end

    voice(["Discord voice"])

    host --> download --> stream --> adapter --> ring --> demux --> mixer --> voice
    adapter -.->|"read error:<br/>try_resume at the same offset"| stream
    cancel(("session<br/>cancel token")) -.-> download
    cancel -.-> stream
```

## Shutdown

```mermaid
sequenceDiagram
    participant Main as main
    participant Bot
    participant Sessions as SessionManager
    participant Shards as run_shard tasks
    participant Players as player_tasks

    Main->>Bot: Ctrl-C, stop
    Bot->>Sessions: close
    Note over Sessions: join now returns Closed
    loop every known guild
        Bot->>Sessions: teardown "shutdown"
        Sessions-->>Players: session token fires
    end
    Bot->>Shards: fire shutdown token
    Shards->>Shards: send close frame, wait up to 5s
    Shards-->>Bot: joined
    Bot->>Players: close tracker, wait up to 12s
    Players-->>Bot: messages show stopped
    Bot-->>Main: return, runtime drops the rest
```