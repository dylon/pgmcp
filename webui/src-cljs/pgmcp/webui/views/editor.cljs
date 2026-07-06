(ns pgmcp.webui.views.editor
  "Embedded lightning-bug (CodeMirror 6 + tree-sitter) source editor for editable
  document text (durable mandate rules, task bodies). All non-serializable
  handles — the shared workspace, the imperative EditorRef, its rxjs
  subscription — live in fx; this view only renders the React component and
  dispatches lifecycle/save events. The editor manages its own DOM internally
  (in node_modules), so no raw DOM work happens in our code."
  (:require [pgmcp.webui.fx :as fx]
            [pgmcp.webui.views.common :as ui]
            [re-frame.core :as rf]
            ["@f1r3fly-io/lightning-bug" :refer [Editor]]))

(def languages
  #js {"markdown" #js {:grammarWasm "/webui/grammars/markdown/grammar.wasm"
                       :highlightsQueryPath "/webui/grammars/markdown/highlights.scm"
                       :extensions #js [".md"]
                       :indentSize 2
                       :fallbackHighlighter "none"}})

(defn editor
  "opts: {:id <editor-kw/str> :text <initial> :uri <inmemory uri>
          :save-url <string> :save-method <\"POST\"|\"PATCH\"> :on-cancel <fn>}."
  [{:keys [id text uri save-url save-method on-cancel]}]
  (let [status @(rf/subscribe [:editor/save-status id])]
    [:div.editor-wrap
     [:div.editor-toolbar
      [ui/toolbar-button {:label (if (= :saving status) "Saving…" "Save")
                          :disabled? (= :saving status)
                          :on-click #(rf/dispatch [:editor/save id save-url save-method])}]
      (when on-cancel
        [ui/toolbar-button {:label "Cancel" :on-click on-cancel}])
      (cond
        (= :done status) [:span.editor-ok "saved"]
        (map? status) [:span.editor-err (:error status)])]
     [:> Editor {:workspace (fx/get-workspace)
                 :languages languages
                 :treeSitterWasm "/webui/grammars/tree-sitter.wasm"
                 :ref (fn [r] (rf/dispatch [:editor/mount {:id id :ref r :text text :uri uri}]))}]]))
