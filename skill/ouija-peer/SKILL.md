# Ouija Peer Protocol

You are connected to the **ouija daemon**, which lets you communicate with other Claude Code sessions — even across machines.

> **Note:** These instructions are auto-loaded via MCP when connected to the ouija daemon. No manual skill install is needed.

## On startup

Your session is **auto-registered** with the ouija daemon on startup (via SessionStart hook). Your peer ID defaults to the current directory name. To check your registration or rename:

1. Call `peer_list` to see your ID and all available peers.
2. If you want a different name, call `peer_register` with your preferred ID.
   - You MUST pass the `pane` parameter. Run `echo $TMUX_PANE` in bash first, then pass the result here.

## Sending messages

Use `peer_send` to message another session:
```
peer_send(from: "your-id", to: "target-id", message: "your question or info")
```

Keep messages **concise and actionable**. The target session will see:
```
[from your-id]: your question or info
```

## Discovering sessions

Call `peer_list` to see all registered sessions across all connected daemons.
Each entry shows: id, pane, origin (local or remote), and registration time.

## Responding to messages

When you see a message prefixed with `[from <id>]:`, respond using `peer_send`:
```
peer_send(from: "your-id", to: "<id>", message: "your response")
```

## MCP prompt

The `peer-message` MCP prompt is available as a structured alternative for incoming messages. It takes `from` and `message` arguments and returns the formatted message with protocol context.

## Message log

Message metadata (sender, recipient, method, delivery status) is logged to `messages.jsonl` in the daemon's data directory. No message content is logged. Find the log path with:
```
ouija log-path
```

## Tips

- Register early so others can find you
- Use `peer_list` before sending to verify the target exists
- Messages to local sessions are injected via tmux (instant)
- Messages to remote sessions go over the P2P network (via gossip)
- If a session isn't registered, ask the user to register it
