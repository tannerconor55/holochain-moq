# Running the Demo on the Windows PC from an Ubuntu Live USB

For the project owner preparing Computer B, a Windows PC. You will start Ubuntu
24.04 LTS **temporarily from a USB stick** ("Try Ubuntu"). Windows is not
installed over, resized, repartitioned or changed. When you shut down and remove
the stick, the PC starts Windows exactly as before.

> The Turbo Kart `.deb` does **not** contain Ubuntu. Download Ubuntu separately
> from the official site, as described below.

---

## A. What you need

| Item | Notes |
|---|---|
| The Windows PC (Computer B) | must have a **64-bit x86 (Intel/AMD) CPU**; check below |
| A USB stick, **8 GB or larger** (16 GB recommended) | **Everything on it will be erased.** Use a stick with nothing you need |
| Internet on the Windows PC | to download Ubuntu (about 6 GB) and Rufus |
| Keyboard, mouse, display | as usual |
| Working Ethernet port or Wi-Fi | to join the same network as Computer A |
| A second USB stick or any way to copy files | to carry the game files from Computer A (the Ubuntu stick also works after Ubuntu is running, if it has space; a second stick is simpler) |
| At least 8 GB RAM recommended | the live session keeps everything in memory |

### Check the CPU type first (in Windows)

Open **Settings → System → About** and look at **System type**:

- `64-bit operating system, x64-based processor`: **OK**, continue.
- `ARM-based processor`: **stop**. The demo package is x86-64 only and will not
  run on this PC.

(Alternative: in PowerShell, `$env:PROCESSOR_ARCHITECTURE` must print `AMD64`.)

### If the PC uses BitLocker

Booting from USB using the one-time boot menu (section D) normally does not trigger
BitLocker. **Changing firmware settings** (Secure Boot, boot order) can make Windows
ask for the BitLocker recovery key at its next start. Before you change anything in
the firmware, make sure you have that recovery key (Microsoft account →
Devices → BitLocker recovery keys). Do not disable encryption.

## B. Download Ubuntu 24.04 LTS and verify it

1. Go to the official site **https://ubuntu.com/download/desktop**.
2. Download **Ubuntu 24.04.x LTS** (the "LTS" desktop release, file name like
   `ubuntu-24.04.x-desktop-amd64.iso`). Use only ubuntu.com or
   **https://releases.ubuntu.com/24.04/**. Do not use third-party download sites.
3. From **https://releases.ubuntu.com/24.04/** also download `SHA256SUMS`.
   It lists the official checksum of each ISO.
4. Verify in **PowerShell** (Start → type "PowerShell"):
   ```powershell
   cd $HOME\Downloads
   Get-FileHash .\ubuntu-24.04.*-desktop-amd64.iso -Algorithm SHA256
   ```
   Compare the long hex value with the line for the same file name in `SHA256SUMS`
   (open it in Notepad; case does not matter). **They must match exactly.**
   If they do not, delete the ISO and download it again.

## C. Create the USB stick in Windows (Rufus)

1. Download **Rufus** from its official site **https://rufus.ie** (the standard or
   portable version).
2. Plug in the USB stick you will erase. **Unplug every other USB drive** so you
   cannot pick the wrong one.
3. Start Rufus and set:
   - **Device**: your USB stick. Check the name and size carefully. It must not be
     your Windows disk, which Rufus normally hides; if you ever see a drive the size
     of your internal disk, stop.
   - **Boot selection**: SELECT → the Ubuntu ISO.
   - **Partition scheme**: GPT (for a PC from about 2013 or later, which starts in
     UEFI mode). Use MBR only for a very old PC.
   - Leave the other options at their defaults. Do not add persistence.
4. Press **START**. If Rufus asks "ISO Image mode or DD Image mode", choose
   **ISO Image mode (Recommended)**. If it offers to download extra files, allow it.
5. Rufus warns that **all data on the USB stick will be destroyed**. Read the drive
   name in the warning one last time, then press OK.
6. Wait for "READY" and close Rufus.

Never use `diskpart`, Disk Management "format", or any partitioning tool on any
internal disk for this demo. Rufus touches only the stick you selected.

## D. Start the PC from the stick (temporary, one time)

1. Leave the stick plugged in. Shut Windows down fully: hold **Shift** while clicking
   **Power → Shut down**. That avoids Windows "fast startup".
2. Turn the PC on and immediately tap the **one-time boot-menu key**. It depends on
   the maker:

   | Maker | Usual key |
   |---|---|
   | Dell | F12 |
   | HP | F9 (or Esc, then F9) |
   | Lenovo | F12 (or the small Novo button) |
   | Acer | F12 |
   | ASUS | F8 or Esc |
   | MSI | F11 |
   | Gigabyte | F12 |
   | Others | watch the first screen for "Boot Menu" |

   A no-keys alternative from Windows: **Settings → System → Recovery → Advanced
   startup → Restart now → Use a device → (your USB stick)**.
3. Pick the entry for the USB stick (often "UEFI: <stick name>"). This choice lasts
   for this start only; it does not change the PC's normal boot order.
4. In the Ubuntu menu choose **"Try or Install Ubuntu"**.
5. When the Ubuntu window appears, choose **"Try Ubuntu"**, then close the installer
   window. **Never choose "Install Ubuntu".**

If Secure Boot is on, Ubuntu 24.04 normally starts anyway, so there is no need to
turn it off. If the PC refuses to boot the stick, see Troubleshooting. Do not
change firmware settings unless you have decided to and have the BitLocker key
(section A).

## E. In the live session

The desktop runs as user `ubuntu` with no password. Open **Terminal**
(Show Apps → Terminal). Check these items:

```sh
uname -m                     # x86_64 (anything else: stop)
lsb_release -ds              # Ubuntu 24.04.x LTS
free -h; df -h /             # memory and free space (the live disk lives in RAM)
timedatectl                  # see "Clock" below
```

The display is working if you can see the desktop. The game needs 3D graphics
(WebGL), and most PCs have it.

**Clock.** Windows keeps the hardware clock in local time and Ubuntu assumes UTC,
so the live session's clock can be hours off until it synchronizes over the
Internet. In `timedatectl` look for **"System clock synchronized: yes"**. Also
compare `date -u` with Computer A; they should agree within a few seconds. If the
network has no Internet, set the right time in Settings → System → Date & Time.

### Copy, verify and install the game

1. Plug in the stick with the three files from Computer A:
   - `dev.holochain-moq.turbo-kart_0.1.0_amd64.deb`
   - `tk-demo.sh`
   - `SHA256SUMS`

   Copy them to **Downloads** using the Files app.
2. In Terminal:
   ```sh
   cd ~/Downloads
   sha256sum -c SHA256SUMS
   ```
   Both lines must say **OK**. The package's SHA-256 must be
   `32b67b3c7092fde6ba7a4f2523f45aff78a761edb4f406dbb213020e703bc6de`
   (see `DEMO-BUILD-MANIFEST.md`). If anything says FAILED, stop and copy again.
3. Install:
   ```sh
   sudo apt install ./dev.holochain-moq.turbo-kart_0.1.0_amd64.deb
   ```
   If apt needs extra libraries it downloads them, which requires Internet. The
   installer also sets up the browser-engine sandbox correctly; nothing else is
   needed.
4. Check that it is the identical build:
   ```sh
   chmod +x tk-demo.sh
   ./tk-demo.sh check
   ```
   `game UI digest` must be `9486315f40dea5ab95086b0a1882935ea92d1e64eebdec4513c8ab003c02472c`,
   and the DNA hashes must match Computer A.

The live session is **temporary**. When you shut down, the installed game, Bob's
player identity and any results vanish, so next time you install again from the
stick. That is expected for this demo; persistence is not set up.

## F. Network

- **Ethernet:** plug in the cable. Ubuntu connects by itself.
- **Wi-Fi:** use the top-right menu → Wi-Fi → choose the **same network as
  Computer A** and enter its password. Use the main home network, **not a guest
  network**. Guest networks usually stop devices from talking to each other
  ("client isolation"), and the game will not find the other PC.
- Your IP address: `ip -4 -o addr show scope global` (for example
  `wlp2s0 192.168.0.61/24`). This is **B_IP**.
- Reach Computer A: `ping -c 3 A_IP`.
- Once Computer A's demo server is running:
  `wget -qO- http://A_IP:47800/health; echo` must print `{}`.

Then continue with **`DEMO-LAN-SETUP.md` §5** (`./tk-demo.sh seed demo 47801`
and `./tk-demo.sh launch A_IP demo`).

## G. Back to Windows

1. Quit the game (close its window).
2. Top-right menu → **Power Off / Log Out → Power Off**.
3. When asked, remove the USB stick and press Enter. Otherwise remove it once the
   PC is off.
4. Turn the PC on. It starts **Windows as usual**. Nothing on the Windows disk was
   changed.

## H. Troubleshooting (non-destructive only)

| Problem | Try |
|---|---|
| USB stick not in the boot menu | Re-plug it into a different port (a rear port on a desktop). Recreate it with Rufus in GPT mode for a UEFI PC. Use the "Advanced startup → Use a device" route. Some PCs list it only as "UEFI: …" |
| Ubuntu does not start, or shows a black screen | In the Ubuntu menu pick **"Try or Install Ubuntu (safe graphics)"**. Re-check the ISO checksum. Recreate the stick in DD mode |
| Secure Boot refuses the stick | Use another stick or port first. Turning off Secure Boot is a firmware change: only do it deliberately, with the BitLocker key at hand, and turn it back on afterwards |
| Wi-Fi not listed | Use Ethernet (simplest). Some Wi-Fi chips need drivers the live USB does not include. Do not install drivers from random sites |
| Display or graphics problems in the game | Try safe graphics. If the game window is black, record the GPU model (`lspci | grep -i vga`) and use the other machine |
| `sha256sum -c` says FAILED | The copy is damaged or different. Copy again from Computer A. Never install a package that fails |
| apt: "wrong architecture" / `uname -m` is not `x86_64` | This PC cannot run the demo package (no ARM build exists) |
| apt: missing dependencies | Connect to the Internet and run the same `sudo apt install ./…deb` again. Do not use `--force` options |
| Game starts but cannot see Alice's lobby | `wget -qO- http://A_IP:47800/health` must print `{}`. Same Wi-Fi, not guest. Was A's firewall rule made with this PC's *current* IP? See DEMO-LAN-SETUP §9 |
| "clock uncertainty …" at race start | Busy or weak Wi-Fi. Move closer, use 5 GHz, or use a cable. Write down the reported value. Also check `timedatectl` (section E) |
| "No space left" | The live session stores everything in RAM. Close other apps, then reboot to clear it. Use a PC with at least 8 GB RAM |
