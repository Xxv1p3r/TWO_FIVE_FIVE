//! Widget Mini Chat flotante/acoplable para comunicarse con el Agente IA Ollama
//! dentro de la interfaz gráfica de Two Five Five (255).

use crate::agent::{AgentUiEvent, AgentWorkerHandle};
use crate::gui_launcher::FloatingWindowState;
use minifb::InputCallback;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Cola segura para recibir caracteres Unicode desde el callback de teclado de `minifb`.
#[derive(Clone, Default)]
pub struct SharedCharQueue(pub Arc<Mutex<VecDeque<char>>>);

impl SharedCharQueue {
    pub fn drain_into(&self) -> Vec<char> {
        let mut q = self.0.lock().unwrap_or_else(|p| p.into_inner());
        q.drain(..).collect()
    }
}

impl InputCallback for SharedCharQueue {
    fn add_char(&mut self, uni_char: u32) {
        if let Some(ch) = char::from_u32(uni_char) {
            if !ch.is_control() && ch != '\x7f' {
                let mut q = self.0.lock().unwrap_or_else(|p| p.into_inner());
                if q.len() < 512 {
                    q.push_back(ch);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatMessageRole {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub role: ChatMessageRole,
    pub text: String,
}

/// Estado del widget Mini Chat de Two Five Five (255).
#[derive(Debug, Clone)]
pub struct MiniChatState {
    pub open: bool,
    pub win: FloatingWindowState,
    pub minimized_rect: (usize, usize, usize, usize),
    pub messages: Vec<ChatEntry>,
    pub input: String,
    pub scroll_offset: usize,
    pub thinking: bool,
    pub ollama_online: bool,
    pub endpoint_label: String,
    pub is_remote: bool,
    pub ollama_url: String,
    pub models: Vec<String>,
    pub active_model: String,
    pub anim_tick: u64,
    pub max_scroll: usize,
}

impl MiniChatState {
    pub fn new(win_w: usize, win_h: usize) -> Self {
        let w = 408usize;
        let h = 472usize;
        let x = win_w.saturating_sub(w + 10);
        let y = win_h.saturating_sub(h + 34);
        let mini_w = 276usize;
        let mini_h = 30usize;
        let mini_x = win_w.saturating_sub(mini_w + 10);
        let mini_y = win_h.saturating_sub(mini_h + 32);

        Self {
            open: true,
            win: FloatingWindowState {
                x,
                y,
                w,
                h,
                is_dragging: false,
                drag_off_x: 0,
                drag_off_y: 0,
            },
            minimized_rect: (mini_x, mini_y, mini_w, mini_h),
            messages: vec![ChatEntry {
                role: ChatMessageRole::System,
                text: "Agente IA Two Five Five listo (servidor por defecto: bogo via Tailscale).".to_string(),
            }],
            input: String::new(),
            scroll_offset: 0,
            thinking: false,
            ollama_online: false,
            endpoint_label: "bogo".to_string(),
            is_remote: true,
            ollama_url: crate::ollama::DEFAULT_BOGO_OLLAMA_URL.to_string(),
            models: Vec::new(),
            active_model: "buscando bogo...".to_string(),
            anim_tick: 0,
            max_scroll: 0,
        }
    }

    pub fn push_message(&mut self, role: ChatMessageRole, text: impl Into<String>) {
        self.messages.push(ChatEntry {
            role,
            text: text.into(),
        });
        if self.messages.len() > 120 {
            let excess = self.messages.len() - 120;
            self.messages.drain(0..excess);
        }
        self.scroll_offset = 0;
    }

    pub fn apply_event(&mut self, ev: AgentUiEvent) {
        match ev {
            AgentUiEvent::StatusUpdate {
                online,
                endpoint_label,
                is_remote,
                url,
                models,
                active_model,
            } => {
                self.ollama_online = online;
                self.endpoint_label = endpoint_label;
                self.is_remote = is_remote;
                self.ollama_url = url;
                self.models = models;
                if !active_model.is_empty() {
                    self.active_model = active_model;
                } else if !online {
                    self.active_model = "sin conexion".to_string();
                }
            }
            AgentUiEvent::SystemNote(note) => {
                self.push_message(ChatMessageRole::System, note);
            }
            AgentUiEvent::Thinking(flag) => {
                self.thinking = flag;
            }
            AgentUiEvent::ToolLog(log) => {
                self.push_message(ChatMessageRole::Tool, log);
            }
            AgentUiEvent::AssistantReply(reply) => {
                self.push_message(ChatMessageRole::Assistant, reply);
            }
            AgentUiEvent::Error(err) => {
                self.push_message(ChatMessageRole::Error, err);
            }
        }
    }

    pub fn submit_prompt(&mut self, prompt: String, worker: &AgentWorkerHandle) {
        let trimmed = prompt.trim().to_string();
        if trimmed.is_empty() || self.thinking {
            return;
        }
        self.push_message(ChatMessageRole::User, trimmed.clone());
        self.thinking = true;
        self.scroll_offset = 0;
        worker.send_prompt(trimmed);
    }

    pub fn contains_point(&self, px: usize, py: usize) -> bool {
        if self.open {
            self.win.contains_point(px, py)
        } else {
            let (x, y, w, h) = self.minimized_rect;
            px >= x && px < x + w && py >= y && py < y + h
        }
    }

    pub fn is_in_drag_header(&self, px: usize, py: usize) -> bool {
        if !self.open {
            return false;
        }
        // Solo el área izquierda del título (sin pisar el selector de servidor/modelo ni botones)
        px >= self.win.x
            && px < self.win.x + 96
            && py >= self.win.y
            && py < self.win.y + 32
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = (self.scroll_offset + lines).min(self.max_scroll);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }
}

/// Primitivas mínimas de dibujo para que el Mini Chat se renderice sobre cualquier `Canvas`.
pub trait ChatCanvas {
    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32);
    fn draw_border(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32);
    fn draw_text(&mut self, x: usize, y: usize, text: &str, color: u32);
    fn draw_text_clipped(&mut self, x: usize, y: usize, max_chars: usize, text: &str, color: u32);
    fn register_btn(&mut self, id: &str, x: usize, y: usize, w: usize, h: usize) -> bool;
    fn draw_drop_shadow(&mut self, x: usize, y: usize, w: usize, h: usize, radius: usize);
}

/// Dibuja el widget Mini Chat (expandido o minimizado) en la capa superior derecha.
pub fn render_mini_chat<C: ChatCanvas>(
    canvas: &mut C,
    st: &mut MiniChatState,
    input_focused: bool,
) {
    if !st.open {
        let (x, y, w, h) = st.minimized_rect;
        canvas.draw_drop_shadow(x, y, w, h, 4);
        let hov = canvas.register_btn("chat_toggle_open", x, y, w, h);
        canvas.fill_rect(x, y, w, h, if hov { 0x245BA3 } else { 0x1D4F91 });
        canvas.draw_border(x, y, w, h, 0x133868);
        let led_col = if st.ollama_online { 0x2ECC71 } else { 0xE74C3C };
        canvas.fill_rect(x + 8, y + 10, 10, 10, led_col);
        let label = if st.thinking {
            format!("IA [{}] pensando...", st.endpoint_label)
        } else {
            format!("IA [{}] [Abrir]", st.endpoint_label)
        };
        canvas.draw_text_clipped(x + 24, y + 7, (w - 30) / 8, &label, 0xFFFFFF);
        return;
    }

    let x = st.win.x;
    let y = st.win.y;
    let w = st.win.w;
    let h = st.win.h;

    canvas.draw_drop_shadow(x, y, w, h, 7);
    canvas.fill_rect(x, y, w, h, 0xFFFFFF);
    canvas.draw_border(x, y, w, h, 0x2A73C7);
    if w > 2 && h > 2 {
        canvas.draw_border(x + 1, y + 1, w - 2, h - 2, 0x7EB0EE);
    }

    // Cabecera (32px)
    canvas.fill_rect(x + 2, y + 2, w - 4, 30, 0x1D4F91);
    let led_col = if st.ollama_online { 0x2ECC71 } else { 0xE74C3C };
    canvas.fill_rect(x + 10, y + 11, 10, 10, led_col);
    canvas.draw_border(x + 10, y + 11, 10, 10, 0x102E54);
    canvas.draw_text(x + 25, y + 9, "IA 255", 0xFFFFFF);

    // Badge clicable de servidor (bogo Tailscale vs local)
    let ep_x = x + 98;
    let ep_w = 62usize;
    let ep_hov = canvas.register_btn("chat_cycle_endpoint", ep_x, y + 5, ep_w, 22);
    let ep_bg = if !st.ollama_online {
        0xA93226
    } else if st.is_remote {
        if ep_hov { 0x1ABC9C } else { 0x148F77 }
    } else if ep_hov {
        0xD68910
    } else {
        0xB9770E
    };
    canvas.fill_rect(ep_x, y + 5, ep_w, 22, ep_bg);
    canvas.draw_border(ep_x, y + 5, ep_w, 22, 0x85C1E9);
    let ep_txt: String = st.endpoint_label.chars().take(6).collect();
    canvas.draw_text(ep_x + 7, y + 8, &ep_txt, 0xFFFFFF);

    // Pill clicable para rotar modelo Ollama
    let model_short: String = st.active_model.chars().take(16).collect();
    let pill_x = ep_x + ep_w + 6;
    let pill_w = x + w.saturating_sub(92) - pill_x;
    let pill_hov = canvas.register_btn("chat_cycle_model", pill_x, y + 5, pill_w, 22);
    canvas.fill_rect(pill_x, y + 5, pill_w, 22, if pill_hov { 0x356EB8 } else { 0x285DA3 });
    canvas.draw_border(pill_x, y + 5, pill_w, 22, 0x6AA3E8);
    canvas.draw_text_clipped(pill_x + 6, y + 8, pill_w / 8 - 1, &model_short, 0xEBF4FF);

    // Botones cabecera: [R] reconectar, [C] limpiar, [_] minimizar
    let btn_specs = [
        ("chat_refresh", x + w - 86, "R", 0x285DA3, 0x356EB8),
        ("chat_clear", x + w - 60, "C", 0x285DA3, 0x356EB8),
        ("chat_toggle_open", x + w - 34, "_", 0x285DA3, 0xC0392B),
    ];
    for (bid, bx, lbl, bg_norm, bg_hov) in btn_specs {
        let hov = canvas.register_btn(bid, bx, y + 5, 22, 22);
        canvas.fill_rect(bx, y + 5, 22, 22, if hov { bg_hov } else { bg_norm });
        canvas.draw_border(bx, y + 5, 22, 22, 0x6AA3E8);
        canvas.draw_text(bx + 7, y + 8, lbl, 0xFFFFFF);
    }

    // Barra de acciones rápidas (26px)
    let bar_y = y + 32;
    canvas.fill_rect(x + 2, bar_y, w - 4, 26, 0xF1F5FA);
    canvas.fill_rect(x + 2, bar_y + 25, w - 4, 1, 0xD8E0EB);
    let chips = [
        ("chat_quick_vms", "VMs", 46usize),
        ("chat_quick_nets", "Redes", 58),
        ("chat_quick_isos", "ISOs", 50),
        ("chat_quick_create", "+Crear VM", 88),
        ("chat_scroll_up", "^", 24),
        ("chat_scroll_down", "v", 24),
    ];
    let mut cx = x + 8;
    for (cid, clbl, cw) in chips {
        let hov = canvas.register_btn(cid, cx, bar_y + 3, cw, 20);
        canvas.fill_rect(cx, bar_y + 3, cw, 20, if hov { 0xDCE8F8 } else { 0xFFFFFF });
        canvas.draw_border(cx, bar_y + 3, cw, 20, 0xB8C5D6);
        let tw = clbl.chars().count() * 8;
        let tx = cx + cw.saturating_sub(tw) / 2;
        canvas.draw_text(tx, bar_y + 5, clbl, 0x1D4F91);
        cx += cw + 5;
    }

    // Área de historial de chat
    let msg_area_x = x + 8;
    let msg_area_y = bar_y + 30;
    let msg_area_w = w.saturating_sub(16);
    let status_strip_h = if st.thinking { 20usize } else { 0usize };
    let msg_area_h = h.saturating_sub(32 + 26 + 44 + status_strip_h + 10);

    canvas.fill_rect(msg_area_x, msg_area_y, msg_area_w, msg_area_h, 0xFAFBFD);
    canvas.draw_border(msg_area_x, msg_area_y, msg_area_w, msg_area_h, 0xD4DCE6);

    // Preparar líneas envueltas de todos los mensajes
    let text_cols = (msg_area_w.saturating_sub(22)) / 8;
    struct RenderLine {
        role: ChatMessageRole,
        line: String,
        is_first: bool,
        is_last: bool,
    }
    let mut all_lines: Vec<RenderLine> = Vec::new();
    for entry in &st.messages {
        let prefix = match entry.role {
            ChatMessageRole::User => "Tu: ",
            ChatMessageRole::Assistant => "IA: ",
            ChatMessageRole::Tool => "[Accion] ",
            ChatMessageRole::System => "[255] ",
            ChatMessageRole::Error => "[Error] ",
        };
        let full = format!("{}{}", prefix, entry.text);
        let wrapped = wrap_text(&full, text_cols);
        let n = wrapped.len();
        for (idx, line) in wrapped.into_iter().enumerate() {
            all_lines.push(RenderLine {
                role: entry.role,
                line,
                is_first: idx == 0,
                is_last: idx + 1 == n,
            });
        }
    }

    let row_h = 18usize;
    let visible_rows = (msg_area_h.saturating_sub(8)) / row_h;
    st.max_scroll = all_lines.len().saturating_sub(visible_rows);
    if st.scroll_offset > st.max_scroll {
        st.scroll_offset = st.max_scroll;
    }
    let end_idx = all_lines.len().saturating_sub(st.scroll_offset);
    let start_idx = end_idx.saturating_sub(visible_rows);

    for (vis_i, item) in all_lines[start_idx..end_idx].iter().enumerate() {
        let ly = msg_area_y + 4 + vis_i * row_h;
        let (bg, border, fg) = match item.role {
            ChatMessageRole::User => (0xDCEBFF, 0x90BDF5, 0x102A4C),
            ChatMessageRole::Assistant => (0xFFFFFF, 0xD4D8E0, 0x1A1E24),
            ChatMessageRole::Tool => (0xEAF7EE, 0x9AD3AC, 0x17592B),
            ChatMessageRole::System => (0xEEF3F9, 0xCBD6E5, 0x3F4D5E),
            ChatMessageRole::Error => (0xFDEDED, 0xF1AEAE, 0x8F1D1D),
        };
        let box_x = msg_area_x + 4;
        let box_w = msg_area_w.saturating_sub(14);
        canvas.fill_rect(box_x, ly, box_w, row_h, bg);
        canvas.fill_rect(box_x, ly, 2, row_h, border);
        if item.is_first {
            canvas.fill_rect(box_x, ly, box_w, 1, border);
        }
        if item.is_last {
            canvas.fill_rect(box_x, ly + row_h - 1, box_w, 1, border);
        }
        canvas.draw_text_clipped(box_x + 6, ly + 1, text_cols, &item.line, fg);
    }

    // Indicador simple de barra de desplazamiento
    if all_lines.len() > visible_rows && msg_area_h > 16 {
        let sb_x = msg_area_x + msg_area_w - 7;
        let sb_h = msg_area_h - 8;
        canvas.fill_rect(sb_x, msg_area_y + 4, 4, sb_h, 0xE6ECF2);
        let thumb_h = ((visible_rows * sb_h) / all_lines.len()).max(16).min(sb_h);
        let travel = sb_h.saturating_sub(thumb_h);
        let ratio_from_top = if st.max_scroll > 0 {
            1.0 - (st.scroll_offset as f32 / st.max_scroll as f32)
        } else {
            1.0
        };
        let thumb_y = msg_area_y + 4 + ((travel as f32) * ratio_from_top) as usize;
        canvas.fill_rect(sb_x, thumb_y, 4, thumb_h, 0x8DA2BC);
    }

    // Franja animada cuando Ollama está procesando
    if st.thinking {
        st.anim_tick = st.anim_tick.wrapping_add(1);
        let dots = match (st.anim_tick / 2) % 4 {
            0 => "",
            1 => ".",
            2 => "..",
            _ => "...",
        };
        let sy = msg_area_y + msg_area_h + 2;
        canvas.draw_text(
            msg_area_x + 4,
            sy,
            &format!("Ollama ({}) procesando{}", model_short, dots),
            0x1D4F91,
        );
    }

    // Barra inferior de escritura + botón Enviar
    let inp_y = y + h - 38;
    let send_w = 72usize;
    let inp_w = w.saturating_sub(send_w + 22);
    let _inp_hov = canvas.register_btn("chat_input_box", x + 8, inp_y, inp_w, 28);
    canvas.fill_rect(x + 8, inp_y, inp_w, 28, 0xFFFFFF);
    canvas.draw_border(
        x + 8,
        inp_y,
        inp_w,
        28,
        if input_focused { 0x3A86FF } else { 0xB8C2D0 },
    );

    let max_inp_cols = inp_w.saturating_sub(14) / 8;
    if st.input.is_empty() {
        let placeholder = if input_focused {
            "Escribe orden para la IA..."
        } else {
            "Clic aqui para hablar con IA..."
        };
        canvas.draw_text_clipped(x + 14, inp_y + 6, max_inp_cols, placeholder, 0x8D99A8);
    } else {
        let chars: Vec<char> = st.input.chars().collect();
        let show = if chars.len() + 1 > max_inp_cols {
            let start = chars.len() + 1 - max_inp_cols;
            chars[start..].iter().collect::<String>()
        } else {
            st.input.clone()
        };
        let shown = if input_focused {
            format!("{}_", show)
        } else {
            show
        };
        canvas.draw_text_clipped(x + 14, inp_y + 6, max_inp_cols, &shown, 0x1A1E24);
    }

    let send_x = x + 8 + inp_w + 6;
    let send_hov = canvas.register_btn("chat_send", send_x, inp_y, send_w, 28);
    let send_bg = if st.thinking {
        0x95A5A6
    } else if send_hov {
        0x28A745
    } else {
        0x1F883D
    };
    canvas.fill_rect(send_x, inp_y, send_w, 28, send_bg);
    canvas.draw_border(send_x, inp_y, send_w, 28, 0x186A3B);
    canvas.draw_text(send_x + 12, inp_y + 6, "Enviar", 0xFFFFFF);
}

/// Procesa clics dentro del Mini Chat. Devuelve `Some(focus_input)` si el botón pertenecía al chat.
pub fn handle_mini_chat_click(
    btn_id: &str,
    st: &mut MiniChatState,
    worker: &AgentWorkerHandle,
) -> Option<bool> {
    match btn_id {
        "chat_toggle_open" => {
            st.open = !st.open;
            Some(st.open)
        }
        "chat_cycle_endpoint" => {
            worker.cycle_endpoint();
            Some(false)
        }
        "chat_cycle_model" => {
            worker.cycle_model();
            Some(false)
        }
        "chat_refresh" => {
            worker.refresh_models();
            st.push_message(ChatMessageRole::System, "Reconectando con servidor Ollama...");
            Some(false)
        }
        "chat_clear" => {
            st.messages.truncate(1);
            st.scroll_offset = 0;
            worker.clear_history();
            Some(false)
        }
        "chat_quick_vms" => {
            st.submit_prompt("Lista todas las maquinas virtuales configuradas y su estado.".to_string(), worker);
            Some(false)
        }
        "chat_quick_nets" => {
            st.submit_prompt("Lista todas las redes virtuales y puertos redirigidos.".to_string(), worker);
            Some(false)
        }
        "chat_quick_isos" => {
            st.submit_prompt("Lista las imagenes ISO disponibles.".to_string(), worker);
            Some(false)
        }
        "chat_quick_create" => {
            st.input = "Crea una VM llamada ".to_string();
            Some(true)
        }
        "chat_scroll_up" => {
            st.scroll_up(4);
            Some(false)
        }
        "chat_scroll_down" => {
            st.scroll_down(4);
            Some(false)
        }
        "chat_input_box" => Some(true),
        "chat_send" => {
            let prompt = std::mem::take(&mut st.input);
            st.submit_prompt(prompt, worker);
            Some(true)
        }
        _ => None,
    }
}

/// Envuelve texto por palabras y saltos de línea para caber en `max_cols` caracteres.
pub fn wrap_text(text: &str, max_cols: usize) -> Vec<String> {
    let cols = max_cols.max(8);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let trimmed_end = raw_line.trim_end_matches('\r');
        if trimmed_end.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in trimmed_end.split(' ') {
            let wlen = word.chars().count();
            if cur_len == 0 {
                if wlen <= cols {
                    cur.push_str(word);
                    cur_len = wlen;
                } else {
                    for chunk_char in word.chars() {
                        if cur_len >= cols {
                            lines.push(cur);
                            cur = String::new();
                            cur_len = 0;
                        }
                        cur.push(chunk_char);
                        cur_len += 1;
                    }
                }
            } else if cur_len + 1 + wlen <= cols {
                cur.push(' ');
                cur.push_str(word);
                cur_len += 1 + wlen;
            } else {
                lines.push(cur);
                cur = String::new();
                cur_len = 0;
                if wlen <= cols {
                    cur.push_str(word);
                    cur_len = wlen;
                } else {
                    for chunk_char in word.chars() {
                        if cur_len >= cols {
                            lines.push(cur);
                            cur = String::new();
                            cur_len = 0;
                        }
                        cur.push(chunk_char);
                        cur_len += 1;
                    }
                }
            }
        }
        if !cur.is_empty() || cur_len == 0 {
            lines.push(cur);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_text_and_long_tokens() {
        let wrapped = wrap_text("Hola mundo desde Two Five Five\nSegunda línea", 14);
        assert!(wrapped.len() >= 3);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 14));

        let long_path = wrap_text("/home/user/isos/debian-12-netinst.iso", 12);
        assert!(long_path.len() >= 3);
        assert!(long_path.iter().all(|l| l.chars().count() <= 12));
    }

    #[test]
    fn test_shared_char_queue_filters_control_chars() {
        let mut q = SharedCharQueue::default();
        q.add_char('H' as u32);
        q.add_char('\n' as u32);
        q.add_char('\x7f' as u32);
        q.add_char('ñ' as u32);
        let chars = q.drain_into();
        assert_eq!(chars, vec!['H', 'ñ']);
    }

    #[test]
    fn test_mini_chat_geometry_and_scroll() {
        let mut st = MiniChatState::new(1140, 720);
        assert!(st.open);
        assert!(st.contains_point(st.win.x + 10, st.win.y + 10));
        assert!(!st.contains_point(10, 10));
        st.max_scroll = 20;
        st.scroll_up(8);
        assert_eq!(st.scroll_offset, 8);
        st.scroll_down(3);
        assert_eq!(st.scroll_offset, 5);
        st.open = false;
        let (mx, my, _, _) = st.minimized_rect;
        assert!(st.contains_point(mx + 5, my + 5));
    }
}
