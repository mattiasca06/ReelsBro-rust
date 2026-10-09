/**
 * @name DunstBridge
 * @author CustomRice
 * @description Bridges Discord pings & DMs to your local Rust Dunst server.
 * @version 2.3.0
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
        this.pendingEmbeds = new Map();
        this.handleUpdate = this.handleUpdate.bind(this);
        this.dispatcher.subscribe("MESSAGE_UPDATE", this.handleUpdate);
        this.outboxTimer = setInterval(() => this.pollOutbox(), 1000);
        BdApi.UI.showToast("DunstBridge Connected to Rust Server!", { type: "success" });
    }

    stop() {
        if (this.dispatcher && this.handleMessage) {
            this.dispatcher.unsubscribe("MESSAGE_CREATE", this.handleMessage);
            this.dispatcher.unsubscribe("MESSAGE_UPDATE", this.handleUpdate);
        }
        clearInterval(this.outboxTimer);
    }

    // Discord's own message-sending function, the one the chat box calls. Looked up lazily so a
    // Discord update that renames it only breaks replies, not notifications.
    getMessageActions() {
        return BdApi.Webpack.getByKeys("sendMessage", "receiveMessage");
    }

    // Sends `content` to a channel through the running Discord client. The channel does not need
    // to be open. Returns null on success or an error string.
    async sendReply(channelId, content) {
        const actions = this.getMessageActions();
        if (!actions) return "Discord's sendMessage function was not found (Discord update?)";
        try {
            // Signature is (channelId, message, waitForChannel, options). The options object must
            // exist or Discord throws on `options.nonce`.
            await actions.sendMessage(channelId, {
                content,
                tts: false,
                invalidEmojis: [],
                validNonShortcutEmojis: []
            }, undefined, {});
            return null;
        } catch (err) {
            console.error("[DunstBridge] sendMessage failed", err);
            return String(err?.message || err);
        }
    }

    // Collects replies typed into the Rust toast and sends them. Quiet when the server is down.
    async pollOutbox() {
        if (this.polling) return;
        this.polling = true;
        try {
            const r = await fetch(`${this.serverUrl}/outbox`);
            const replies = await r.json();
            for (const { channel_id, content } of replies) {
                const error = await this.sendReply(channel_id, content);
                if (error) BdApi.UI.showToast(`❌ Reply not sent: ${error}`, { type: "error" });
            }
        } catch (_) {
            // Rust app not running; try again next tick.
        } finally {
            this.polling = false;
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

    // Discord stores mentions as raw ids (<@123>, <@&role>, <#channel>, <:emoji:id>). Turn them into
    // readable text for the toast. Unknown ids are left as they are.
    resolveMentions(message) {
        const guildId = message.guild_id;
        const channelStore = BdApi.Webpack.getStore("ChannelStore");
        const roleStore = BdApi.Webpack.getStore("GuildRoleStore");
        const nameOf = u => u?.global_name || u?.username;

        return (message.content || "")
            .replace(/<@!?(\d+)>/g, (raw, id) => {
                const name = nameOf(message.mentions?.find(u => u.id === id)) || nameOf(this.userStore.getUser(id));
                return name ? `@${name}` : raw;
            })
            .replace(/<@&(\d+)>/g, (raw, id) => {
                const role = guildId && roleStore?.getRole?.(guildId, id);
                return role?.name ? `@${role.name}` : raw;
            })
            .replace(/<#(\d+)>/g, (raw, id) => {
                const channel = channelStore?.getChannel?.(id);
                return channel?.name ? `#${channel.name}` : raw;
            })
            .replace(/<a?:(\w+):\d+>/g, ":$1:");
    }

    // First image or gif of a message: an uploaded image, an image link, or a gif link (Tenor, Klipy,
    // Giphy...). Gif embeds are delivered by Discord as a looping muted clip ("gifv"). Plain videos
    // are ignored. Returns {url, kind} or null.
    extractMedia(message) {
        for (const a of message.attachments || []) {
            if (a.content_type?.startsWith("image/") || /\.(png|jpe?g|gif|webp)$/i.test(a.filename || "")) {
                return { url: a.url, kind: "image" };
            }
        }
        for (const e of message.embeds || []) {
            const provider = (e.provider?.name || "").toLowerCase();
            const isGifSite = ["tenor", "giphy", "klipy"].includes(provider);
            if ((e.type === "gifv" || isGifSite) && e.video?.url) return { url: e.video.url, kind: "gifv" };
            if (e.type === "image") return { url: e.thumbnail?.url || e.url, kind: "image" };
            if (e.type !== "video" && (e.image?.url || e.thumbnail?.url)) {
                return { url: e.image?.url || e.thumbnail.url, kind: "image" };
            }
        }
        return null;
    }

    // Link embeds are built by Discord after the message arrives, so they show up in MESSAGE_UPDATE.
    handleUpdate({ message }) {
        const pending = message?.id && this.pendingEmbeds.get(message.id);
        if (!pending) return;
        const media = this.extractMedia(message);
        if (!media) return;
        this.pendingEmbeds.delete(message.id);
        this.sendToRust({
            ...pending.base,
            content: "",
            media_update: true,
            media_url: media.url,
            media_kind: media.kind
        });
    }

    handleMessage({ message }) {
        if (!message || !this.userStore) return;
        const currentUser = this.userStore.getCurrentUser();

        if (!currentUser || message.author?.id === currentUser.id) return;

        const isPinged = message.mentions?.some(u => u.id === currentUser.id) || message.mention_everyone;
        const isDM = !message.guild_id;

        if (isPinged || isDM) {
            const media = this.extractMedia(message);
            const text = this.resolveMentions(message);
            const base = {
                author: message.author.global_name || message.author.username || "Someone",
                channel_id: message.channel_id,
                avatar: message.author.avatar
                    ? `https://cdn.discordapp.com/avatars/${message.author.id}/${message.author.avatar}.png`
                    : "https://cdn.discordapp.com/embed/avatars/0.png",
                sound_path: this.soundPath,
                accent_color: message.guild_id ? "#cba6f7" : "#f38ba8",
                position: message.guild_id ? "top-right" : "top-left"
            };
            this.sendToRust({
                ...base,
                content: text || (media ? "" : "Sent an attachment"),
                media_url: media?.url,
                media_kind: media?.kind
            });

            // A link with no embed yet may get one in a moment; remember it for MESSAGE_UPDATE.
            if (!media && /https?:\/\//.test(text)) {
                const now = Date.now();
                for (const [id, p] of this.pendingEmbeds) if (now - p.t > 30000) this.pendingEmbeds.delete(id);
                this.pendingEmbeds.set(message.id, { t: now, base });
            }
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

        // Reply tests: check that Discord's send function exists, then send a real message to a
        // channel by ID (right-click a channel or DM with Developer Mode on, "Copy Channel ID").
        // Open a different chat first to prove the target doesn't have to be on screen.
        const checkBtn = document.createElement("button");
        checkBtn.innerText = "🔍 Check Reply Send Function";
        checkBtn.className = "bd-button";
        checkBtn.style.padding = "10px 16px";
        checkBtn.style.borderRadius = "8px";
        checkBtn.style.cursor = "pointer";
        checkBtn.onclick = () => {
            const actions = this.getMessageActions();
            if (actions && typeof actions.sendMessage === "function") {
                BdApi.UI.showToast("✅ sendMessage found, replies from the toast can work", { type: "success" });
            } else {
                BdApi.UI.showToast("❌ sendMessage not found, Discord changed its internals", { type: "error" });
            }
        };

        const channelInput = document.createElement("input");
        channelInput.type = "text";
        channelInput.placeholder = "Channel ID to send the test message to";
        channelInput.style.padding = "8px 12px";
        channelInput.style.borderRadius = "8px";

        const replyBtn = document.createElement("button");
        replyBtn.innerText = "💬 Send Test Reply To Channel ID";
        replyBtn.className = "bd-button";
        replyBtn.style.padding = "10px 16px";
        replyBtn.style.borderRadius = "8px";
        replyBtn.style.cursor = "pointer";
        replyBtn.onclick = async () => {
            const id = channelInput.value.trim();
            if (!/^\d{15,25}$/.test(id)) {
                BdApi.UI.showToast("❌ Enter a numeric channel ID first", { type: "error" });
                return;
            }
            const error = await this.sendReply(id, "DunstBridge reply test");
            if (error) BdApi.UI.showToast(`❌ Send failed: ${error}`, { type: "error" });
            else BdApi.UI.showToast("✅ Sent. Check that channel.", { type: "success" });
        };

        // Fake DM with an image (text + picture), to check the toast preview and the click-to-zoom viewer.
        const imageBtn = document.createElement("button");
        imageBtn.innerText = "🖼️ Simulate Image DM";
        imageBtn.className = "bd-button";
        imageBtn.style.padding = "10px 16px";
        imageBtn.style.borderRadius = "8px";
        imageBtn.style.cursor = "pointer";
        imageBtn.onclick = () => {
            this.sendToRust({
                author: "PicBro",
                content: "look at this",
                avatar: "https://cdn.discordapp.com/embed/avatars/3.png",
                sound_path: this.soundPath,
                accent_color: "#fab387",
                media_url: "https://picsum.photos/id/1015/1600/1000",
                media_kind: "image"
            });
        };

        panel.appendChild(singleBtn);
        panel.appendChild(spamBtn);
        panel.appendChild(imageBtn);
        panel.appendChild(ytdlpBtn);
        panel.appendChild(reelBtn);
        panel.appendChild(checkBtn);
        panel.appendChild(channelInput);
        panel.appendChild(replyBtn);
        return panel;
    }
};  