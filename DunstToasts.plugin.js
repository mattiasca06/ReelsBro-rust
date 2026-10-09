/**
 * @name DunstBridge
 * @author CustomRice
 * @description Bridges Discord pings & DMs to your local Rust Dunst server.
 * @version 2.1.0
 */

module.exports = class DunstBridge {
    serverUrl = "http://127.0.0.1:8999";
    soundPath = "C:\\Users\\matti\\Documents\\cosa.mp3";
    testReelUrl = "https://www.instagram.com/reel/DeP0ro-Az15/?utm_source=ig_web_copy_link&dlrf=NTc4MTIwNjQ2YQ==";

    start() {
        this.dispatcher = this.getDispatcher();
        this.userStore = BdApi.Webpack.getStore("UserStore");

        if (!this.dispatcher) {
            console.error("[DunstBridge] Could not find Dispatcher module.");
            return;
        }

        this.handleMessage = this.handleMessage.bind(this);
        this.dispatcher.subscribe("MESSAGE_CREATE", this.handleMessage);
        BdApi.UI.showToast("DunstBridge Connected to Rust Server!", { type: "success" });
    }

    stop() {
        if (this.dispatcher && this.handleMessage) {
            this.dispatcher.unsubscribe("MESSAGE_CREATE", this.handleMessage);
        }
    }

    getDispatcher() {
        return (
            BdApi.Webpack.getModule(m => m?.subscribe && m?.dispatch, { searchExports: true }) ||
            BdApi.Webpack.getByKeys("dispatch", "subscribe") ||
            BdApi.Webpack.getModule(m => m?.default?.dispatch && m?.default?.subscribe)?.default
        );
    }

    sendToRust(payload) {
        fetch(this.serverUrl, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(payload)
        }).catch(err => {
            console.error("[DunstBridge] Make sure cargo run is running!", err);
        });
    }

    handleMessage({ message }) {
        if (!message || !this.userStore) return;
        const currentUser = this.userStore.getCurrentUser();
        
        if (!currentUser || message.author?.id === currentUser.id) return;

        const isPinged = message.mentions?.some(u => u.id === currentUser.id) || message.mention_everyone;
        const isDM = !message.guild_id;

        if (isPinged || isDM) {
            this.sendToRust({
                author: message.author.global_name || message.author.username || "Someone",
                content: message.content || "Sent an attachment",
                avatar: message.author.avatar
                    ? `https://cdn.discordapp.com/avatars/${message.author.id}/${message.author.avatar}.png`
                    : "https://cdn.discordapp.com/embed/avatars/0.png",
                sound_path: this.soundPath,
                accent_color: message.guild_id ? "#cba6f7" : "#f38ba8",
                position: message.guild_id ? "top-right" : "top-left"
            });
        }
    }

    // Asks the Rust server to verify yt-dlp. Returns true if installed and not known to be outdated.
    async assertYtdlp(quietOnPass = false) {
        const toast = (msg, type) => BdApi.UI.showToast(msg, { type });
        let res;
        try {
            const r = await fetch(`${this.serverUrl}/health/ytdlp`);
            res = await r.json();
        } catch (err) {
            console.error("[DunstBridge] yt-dlp check failed", err);
            toast("❌ Can't reach the Rust server (is it running? is it the new build?)", "error");
            return false;
        }

        if (!res.installed) {
            console.error("[DunstBridge] ASSERT FAILED: yt-dlp not installed", res);
            toast(`❌ yt-dlp is not installed: ${res.error}`, "error");
            return false;
        }
        if (res.up_to_date === false) {
            console.error("[DunstBridge] ASSERT FAILED: yt-dlp outdated", res);
            toast(`❌ yt-dlp is outdated (${res.version}, latest ${res.latest}). Run: yt-dlp -U`, "error");
            return false;
        }
        if (res.up_to_date === null) {
            toast(`⚠️ yt-dlp ${res.version} installed, but couldn't check latest: ${res.error}`, "warning");
            return true;
        }
        if (!quietOnPass) toast(`✅ yt-dlp ${res.version} is installed and up to date`, "success");
        return true;
    }

    // --- SETTINGS PANEL WITH AUTOMATED TEST SUITE ---
    getSettingsPanel() {
        const panel = document.createElement("div");
        panel.style.display = "flex";
        panel.style.flexDirection = "column";
        panel.style.gap = "12px";
        panel.style.padding = "14px";

        // Button 1: Single Ping
        const singleBtn = document.createElement("button");
        singleBtn.innerText = "⚡ Single Ping Test";
        singleBtn.className = "bd-button";
        singleBtn.style.padding = "10px 16px";
        singleBtn.style.borderRadius = "8px";
        singleBtn.style.cursor = "pointer";
        singleBtn.onclick = () => {
            this.sendToRust({
                author: "Matti",
                content: "Hey, just a single ping test!",
                avatar: "https://cdn.discordapp.com/embed/avatars/0.png",
                sound_path: this.soundPath,
                accent_color: "#a6e3a1",
                position: "top-right"
            });
        };

        // Button 2: Automated DM Spam + Essay Simulation
        const spamBtn = document.createElement("button");
        spamBtn.innerText = "💬 Simulate WhatsApp DM Spam (3 Messages + Essay)";
        spamBtn.className = "bd-button bd-button-filled";
        spamBtn.style.padding = "10px 16px";
        spamBtn.style.borderRadius = "8px";
        spamBtn.style.cursor = "pointer";
        spamBtn.style.backgroundColor = "#cba6f7";
        spamBtn.style.color = "#11111b";
        spamBtn.style.fontWeight = "bold";

        spamBtn.onclick = () => {
            const avatar = "https://cdn.discordapp.com/embed/avatars/1.png";
            const author = "BestFriend";
            const accent = "#cba6f7";

            // 1st Message (Immediate)
            this.sendToRust({
                author, avatar, accent_color: accent, sound_path: this.soundPath,
                content: "yo matti you there? 👀"
            });

            // 2nd Message (1.4s later)
            setTimeout(() => {
                this.sendToRust({
                    author, avatar, accent_color: accent, sound_path: this.soundPath,
                    content: "look at this notification rice we just built in Rust"
                });
            }, 1400);

            // 3rd Message - The Essay (3.2s later)
            setTimeout(() => {
                this.sendToRust({
                    author, avatar, accent_color: accent, sound_path: this.soundPath,
                    content: "bro the window literally expands dynamically, uses WhatsApp style bubble appending without reloading the list, and gives us enough reading time based on character count so we can actually read long paragraphs like this without it closing early! 🔥"
                });
            }, 3200);
        };

        // Button 3: yt-dlp assert (installed + up to date)
        const ytdlpBtn = document.createElement("button");
        ytdlpBtn.innerText = "🔧 Assert yt-dlp Installed & Up To Date";
        ytdlpBtn.className = "bd-button";
        ytdlpBtn.style.padding = "10px 16px";
        ytdlpBtn.style.borderRadius = "8px";
        ytdlpBtn.style.cursor = "pointer";
        ytdlpBtn.onclick = () => this.assertYtdlp();

        // Button 4: Fake a reel DM to check the reel player loads
        const reelBtn = document.createElement("button");
        reelBtn.innerText = "🎬 Simulate Reel DM (Load Test)";
        reelBtn.className = "bd-button";
        reelBtn.style.padding = "10px 16px";
        reelBtn.style.borderRadius = "8px";
        reelBtn.style.cursor = "pointer";
        reelBtn.onclick = async () => {
            if (!(await this.assertYtdlp(true))) return;
            this.sendToRust({
                author: "ReelBro",
                content: this.testReelUrl,
                avatar: "https://cdn.discordapp.com/embed/avatars/2.png",
                sound_path: this.soundPath,
                accent_color: "#89b4fa",
                test: true
            });
            BdApi.UI.showToast("Reel sent. If the player doesn't open, check the error toast.", { type: "info" });
        };

        panel.appendChild(singleBtn);
        panel.appendChild(spamBtn);
        panel.appendChild(ytdlpBtn);
        panel.appendChild(reelBtn);
        return panel;
    }
};  