pub const FRAME_HELPERS: &str = r##"
function emissaryClean(text) {
    return String(text || "").replace(/\s+/g, " ").trim();
}

function emissaryFrameDocuments() {
    const contexts = [];
    const seen = new Set();

    function visit(doc, path, frameElement) {
        if (!doc || seen.has(doc)) return;
        seen.add(doc);
        contexts.push({ doc, path, frameElement });

        const frames = Array.from(doc.querySelectorAll("iframe,frame"));
        frames.forEach((frame, index) => {
            try {
                const childDoc = frame.contentDocument || (frame.contentWindow && frame.contentWindow.document);
                if (childDoc) visit(childDoc, path.concat(index + 1), frame);
            } catch (_) {
                // Cross-origin frames are intentionally skipped by the browser security model.
            }
        });
    }

    visit(document, [], null);
    return contexts;
}

function emissaryFrameName(path) {
    return path.length ? `iframe:${path.join(".")}` : null;
}

function emissaryVisible(el) {
    const view = el.ownerDocument.defaultView || window;
    const rect = el.getBoundingClientRect();
    const style = view.getComputedStyle(el);
    return rect.width > 1 &&
        rect.height > 1 &&
        rect.bottom >= 0 &&
        rect.right >= 0 &&
        rect.top <= view.innerHeight &&
        rect.left <= view.innerWidth &&
        style.visibility !== "hidden" &&
        style.display !== "none" &&
        style.opacity !== "0";
}

function emissaryFindByAttribute(refId, attribute) {
    for (const ctx of emissaryFrameDocuments()) {
        const el = Array.from(ctx.doc.querySelectorAll(`[${attribute}]`))
            .find((candidate) => candidate.getAttribute(attribute) === refId);
        if (el) return { ...ctx, el };
    }
    return null;
}

function emissaryFindRef(refId) {
    return emissaryFindByAttribute(refId, "data-emissary-ref");
}

function emissaryScrollIntoView(ctx, el) {
    if (ctx.frameElement) {
        ctx.frameElement.scrollIntoView({ block: "center", inline: "center" });
    }
    el.scrollIntoView({ block: "center", inline: "center" });
}
"##;

#[cfg(test)]
mod tests {
    use super::FRAME_HELPERS;

    #[test]
    fn frame_helpers_traverse_accessible_iframes_and_refs() {
        assert!(FRAME_HELPERS.contains("contentDocument"));
        assert!(FRAME_HELPERS.contains("emissaryFrameDocuments"));
        assert!(FRAME_HELPERS.contains("emissaryFindRef"));
    }
}
