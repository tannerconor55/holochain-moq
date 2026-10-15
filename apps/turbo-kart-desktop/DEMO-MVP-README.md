# Turbo Kart Rally: Multiplayer Demo v0.1

Turbo Kart Rally is an arcade kart racer. In this demo **two people race each
other** on two computers in the same home network, with **six computer-driven
karts** filling out an eight-kart grid. There is no game server: the two computers
connect to each other directly.

## What the demo supports

| | |
|---|---|
| Players | **2 humans + 6 AI karts** |
| Multiplayer track | **Palm Cove Circuit**, 3 laps |
| Single-player tracks | Palm Cove Circuit, Mirror Cove Circuit, Coastal Switchbacks |
| Computer | 64-bit Intel/AMD PC running **Ubuntu Linux** (24.04 or newer). The Windows PC runs Ubuntu from a USB stick for this demo |
| Network | both PCs on the **same home network** (cable or Wi-Fi). Internet play is not supported yet |

Both computers must have **exactly the same game file** installed. The person
running the demo checks this for you.

## Starting the game

The demo organizer starts the game on each computer, because the demo needs one
network setting. A window titled **Turbo Kart Rally** opens. The first start
takes a little longer while the game sets up your player identity.

## Racing each other

**Player 1 (host)**

1. Choose **Multiplayer**.
2. Choose **Create lobby**: give it a name, keep **Palm Cove** and **3 laps**, pick
   your character, and confirm.
3. You are now the **host** of the room, and the room shows you as host. Wait for
   player 2 to appear.

**Player 2**

1. Choose **Multiplayer**.
2. Press **Refresh** until player 1's lobby appears. This can take a few seconds.
3. Select it, pick your character, and press **Join lobby**.

**Starting**

- When both players are listed in the room, the host presses **Start Race**.
- Both screens load Palm Cove, then count down together. Drive on **GO**.
- Only the host can start the race.

## Controls

| Action | Keyboard | Gamepad |
|---|---|---|
| Accelerate | ↑ or W | A / RT |
| Brake / reverse | ↓ or S | B / LT |
| Steer | ← → or A D | D-pad |
| Drift | Space or Right Shift | RB / X |
| Look back | C | stick press |
| Pause / back | Esc or P | Start / Back |
| Mute | M | — |

Items are switched off in multiplayer for this demo.

## Results and racing again

- After three laps the **results** screen shows the finishing order. Both computers
  should show the **same order**.
- Return to the menu, go to **Multiplayer**, and the host creates a **new lobby**.
  Player 2 refreshes and joins it again.

## Known limitations

- Two human players only. Palm Cove is the only multiplayer track; Mirror Cove and
  Coastal Switchbacks are single-player only.
- No items or obstacles in multiplayer.
- Results are not saved; there is no leaderboard yet.
- If the host's game closes or crashes, the race ends for both players. Start a new
  lobby.
- Some characters still have placeholder names (A–E).
- Same home network only. Guest Wi-Fi often blocks the two computers from seeing
  each other.

## If something goes wrong

| What you see | Try |
|---|---|
| The lobby doesn't show up | Wait a few seconds and press **Refresh** again. Check both computers are on the same Wi-Fi (not "guest") |
| "Request timed out" or "has not reached this desktop yet" | Press Refresh or Check lobby and try once more. Do not create several lobbies |
| The race doesn't start / mentions "clock uncertainty" | The Wi-Fi is too slow right now. Move closer to the router or use a cable, then make a new lobby |
| The game window is black | Ask the demo organizer; the graphics on that PC may not be supported |
| Anything else | Close the game on both computers and start again |
