(ns pgmcp.webui.fx
  (:require [clojure.string :as str]
            [pgmcp.webui.render :as render]
            [re-frame.core :as rf]
            ["unified" :refer [unified]]
            ["remark-parse" :default remark-parse]
            ["remark-gfm" :default remark-gfm]
            ["remark-rehype" :default remark-rehype]
            ["rehype-slug" :default rehype-slug]
            ["web-tree-sitter" :refer [Language Parser Query]]
            ["@f1r3fly-io/lightning-bug" :refer [createWorkspace]]))

(defonce socket (atom nil))
(defonce keyboard-installed? (atom false))

;; Lightning-bug embedded editor: one shared workspace (non-serializable, edge)
;; created lazily; the per-editor imperative refs live in `editor-refs`.
(defonce lb-workspace (delay (createWorkspace)))
(defonce editor-refs (atom {}))

(defn get-workspace [] @lb-workspace)

(rf/reg-cofx
 :pgmcp/now
 (fn [cofx _]
   (assoc cofx :pgmcp/now (.now js/Date))))

(defn with-auth-header [options token]
  (let [base (or options {})]
    (if (str/blank? (or token ""))
      base
      (assoc base :headers (assoc (or (:headers base) {})
                                  "authorization" (str "Bearer " token))))))

(defn fetch-json
  ([url] (fetch-json url nil nil))
  ([url options] (fetch-json url options nil))
  ([url options token]
   (let [request (clj->js (with-auth-header options token))]
     (-> (.fetch js/window url request)
         (.then
          (fn [response]
            (if (.-ok response)
              (.json response)
              (do
                (when (= 401 (.-status response))
                  (rf/dispatch [:machine/dispatch
                                {:type :session/expired
                                 :message "Session expired — re-enter the webui token."}]))
                (-> (.text response)
                    (.then
                     (fn [text]
                       (throw (js/Error.
                               (or (not-empty text)
                                   (str (.-status response) " " (.-statusText response))))))))))))))))

(defn url-params [params]
  (let [out (js/URLSearchParams.)]
    (doseq [[k v] params]
      (when (some? v)
        (.set out (name k) (str v))))
    out))

(defn websocket-open? [ws]
  (and ws (= (.-readyState ws) (.-OPEN js/WebSocket))))

(defn hello-payload [{:keys [since topics]}]
  {"type" "hello"
   "since" (or since 0)
   "topics" (or topics [])})

(defn send-hello! [ws subscription]
  (when (websocket-open? ws)
    (.send ws (.stringify js/JSON (clj->js (hello-payload subscription))))))

(defn websocket-url [{:keys [token since topics]}]
  (let [location (.-location js/window)
        scheme (if (= "https:" (.-protocol location)) "wss:" "ws:")
        url (js/URL. (str scheme "//" (.-host location) "/webui/ws"))]
    (.set (.-searchParams url) "since" (str (or since 0)))
    (when (seq topics)
      (.set (.-searchParams url) "topics" (str/join "," topics)))
    (when-not (str/blank? (or token ""))
      (.set (.-searchParams url) "token" token))
    (.toString url)))

(rf/reg-fx
 :pgmcp/remember-token
 (fn [token]
   (if (str/blank? (or token ""))
     (.removeItem (.-localStorage js/window) "pgmcp.webui.token")
     (.setItem (.-localStorage js/window) "pgmcp.webui.token" token))))

(rf/reg-fx
 :pgmcp/install-keyboard
 (fn [_]
   (when-not @keyboard-installed?
     (reset! keyboard-installed? true)
     (.addEventListener
      js/window
      "keydown"
      (fn [event]
        (when (and (.-altKey event) (= "ArrowLeft" (.-key event)))
          (.preventDefault event)
          (rf/dispatch [:machine/dispatch {:type :nav/back}])))
      true))))

(rf/reg-fx
 :pgmcp/copy
 (fn [text]
   (when-let [clip (.-clipboard js/navigator)]
     (.writeText clip (str text)))))

(defn md-processor []
  (-> (unified)
      (.use remark-parse)
      (.use remark-gfm)
      (.use remark-rehype)
      (.use rehype-slug)))

;; Markdown → hast (unified/remark/rehype) → hiccup (pure render/hast->hiccup).
;; No rehype-raw: embedded raw HTML is dropped (XSS-safe), and the result is
;; hiccup, never an HTML string, so the no-raw-HTML gate stays intact.
(rf/reg-fx
 :pgmcp/render-md
 (fn [{:keys [id text]}]
   (let [proc (md-processor)]
     (-> (.run proc (.parse proc (or text "")))
         (.then (fn [hast]
                  (rf/dispatch [:render/store id (render/hast->hiccup hast)])))
         (.catch (fn [e] (js/console.warn "markdown render failed:" e)))))))

(def grammar-catalog
  {"markdown" ["/webui/grammars/markdown/grammar.wasm" "/webui/grammars/markdown/highlights.scm"]
   "json" ["/webui/grammars/json/grammar.wasm" "/webui/grammars/json/highlights.scm"]
   "rust" ["/webui/grammars/rust/grammar.wasm" "/webui/grammars/rust/highlights.scm"]
   "clojure" ["/webui/grammars/clojure/grammar.wasm" "/webui/grammars/clojure/highlights.scm"]
   "python" ["/webui/grammars/python/grammar.wasm" "/webui/grammars/python/highlights.scm"]
   "bash" ["/webui/grammars/bash/grammar.wasm" "/webui/grammars/bash/highlights.scm"]
   "toml" ["/webui/grammars/toml/grammar.wasm" "/webui/grammars/toml/highlights.scm"]})

(defonce ts-ready
  (delay (.init Parser #js {:locateFile (fn [_ _] "/webui/grammars/tree-sitter.wasm")})))

(defonce grammar-cache (atom {}))

(defn load-grammar [lang]
  (when-let [[wasm scm] (get grammar-catalog lang)]
    (or (get @grammar-cache lang)
        (let [p (-> @ts-ready
                    (.then (fn [_]
                             (js/Promise.all
                              #js [(.load Language wasm)
                                   (-> (js/fetch scm) (.then #(.text %)))])))
                    (.then (fn [arr]
                             (let [language (aget arr 0)]
                               {:language language :query (Query. language (aget arr 1))}))))]
          (swap! grammar-cache assoc lang p)
          p))))

(defn capture-spans [^js tree ^js query]
  (->> (array-seq (.captures query (.-rootNode tree)))
       (keep (fn [cap]
               (let [node (.-node cap)
                     cls (render/class-for (.-name cap))]
                 (when cls
                   {:from (.-startIndex node) :to (.-endIndex node) :class cls}))))))

;; Tree-sitter read-only highlighting: load the grammar (cached), parse, run its
;; highlights query → capture spans → hiccup (render/spans->hiccup). No grammar
;; for a language, or any failure → leaves the code-view plain (no dispatch).
(rf/reg-fx
 :pgmcp/highlight-code
 (fn [{:keys [id code language]}]
   (if-let [gp (load-grammar (some-> language str str/lower-case not-empty))]
     (-> gp
         (.then (fn [loaded]
                  (let [parser (Parser.)]
                    (.setLanguage parser (:language loaded))
                    (let [tree (.parse parser (or code ""))
                          spans (capture-spans tree (:query loaded))]
                      (rf/dispatch [:render/store id (render/spans->hiccup code spans)])))))
         ;; console.debug (not warn): a grammar/WASM load failure degrades the
         ;; view to plain text, which is a graceful, expected fallback.
         (.catch (fn [e] (js/console.debug "code highlight failed:" e))))
     nil)))

;; Lightning-bug editor lifecycle. The imperative EditorRef is stored in
;; editor-refs (edge, non-serializable) keyed by editor id; on ready the
;; document is opened; save reads the current text and POSTs it.
(rf/reg-fx
 :pgmcp/editor-mount
 (fn [{:keys [id ref text uri]}]
   (if (nil? ref)
     (swap! editor-refs dissoc id)
     (do
       (swap! editor-refs assoc id ref)
       (let [open! (fn [] (.openDocument ref (or uri "inmemory://doc.md") (str text) "markdown"))]
         (if (.isReady ref)
           (open!)
           (.subscribe (.getEvents ref)
                       (fn [ev] (when (= "ready" (.-type ev)) (open!))))))))))

(rf/reg-fx
 :pgmcp/editor-save
 (fn [{:keys [id url method token]}]
   (when-let [ref (get @editor-refs id)]
     (let [text (.getText ref)]
       (-> (fetch-json url
                       {:method (or method "POST")
                        :headers {"content-type" "application/json"}
                        :body (.stringify js/JSON #js {"text" text})}
                       token)
           (.then #(rf/dispatch [:editor/save-done id nil]))
           (.catch #(rf/dispatch [:editor/save-done id (.-message %)])))))))

;; Lightweight polling for live read panes (htop-style Resources, live Clients).
;; setInterval re-dispatches a machine load event; the pane clears its poller on
;; unmount. Push-liveness (Events) rides the websocket; this covers snapshot
;; panes whose data changes continuously.
(defonce pollers (atom {}))

(rf/reg-fx
 :pgmcp/poll-start
 (fn [{:keys [id event interval-ms]}]
   (when-let [h (get @pollers id)] (js/clearInterval h))
   (swap! pollers assoc id (js/setInterval #(rf/dispatch event) interval-ms))))

(rf/reg-fx
 :pgmcp/poll-stop
 (fn [{:keys [id]}]
   (when-let [h (get @pollers id)]
     (js/clearInterval h)
     (swap! pollers dissoc id))))

;; Generic operator write (work-item transitions, mandate CRUD). POST/PATCH a
;; JSON body with the bearer token; on success dispatch on-done with nil error,
;; on failure with the message. 401 → :session/expired is handled in fetch-json.
(rf/reg-fx
 :pgmcp/write
 (fn [{:keys [url method body token on-done]}]
   (-> (fetch-json url
                   {:method (or method "POST")
                    :headers {"content-type" "application/json"}
                    :body (when body (.stringify js/JSON (clj->js body)))}
                   token)
       (.then (fn [_] (rf/dispatch (conj on-done nil))))
       (.catch (fn [e] (rf/dispatch (conj on-done (.-message e))))))))

(rf/reg-fx
 :pgmcp/apply-theme
 (fn [theme]
   (let [t (if (= theme :light) "light" "dark")]
     ;; setAttribute (not dataset.theme) so :advanced compilation cannot munge
     ;; the property name; matches the CSS `:root[data-theme="light"]` selector.
     (.setAttribute (.-documentElement js/document) "data-theme" t)
     (.setItem (.-localStorage js/window) "pgmcp.webui.theme" t))))

(rf/reg-fx
 :pgmcp/check-session
 (fn [{:keys [token]}]
   (-> (.fetch js/window "/api/stats?kind=counters"
               (clj->js (with-auth-header nil token)))
       (.then
        (fn [response]
          (if (.-ok response)
            (rf/dispatch [:machine/dispatch {:type :session/ok}])
            (rf/dispatch [:machine/dispatch
                          {:type :session/unauthorized
                           :message (if (= 401 (.-status response))
                                      "Invalid or missing webui token."
                                      (str "Daemon returned " (.-status response) " " (.-statusText response) "."))}]))))
       (.catch
        (fn [_]
          (rf/dispatch [:machine/dispatch
                        {:type :session/unauthorized
                         :message "Could not reach the daemon."}]))))))

(rf/reg-fx
 :pgmcp/start-app
 (fn [_]
   (rf/dispatch [:machine/dispatch {:type :stats/load :kind :status}])
   (rf/dispatch [:machine/dispatch {:type :ws/connect}])))

(rf/reg-fx
 :pgmcp/fetch-stats
 (fn [{:keys [kind request-id token]}]
   (-> (fetch-json (str "/api/stats?kind=" (js/encodeURIComponent (name kind))) nil token)
       (.then #(rf/dispatch [:machine/dispatch
                             {:type :stats/loaded
                              :kind kind
                              :request-id request-id
                              :payload (js->clj % :keywordize-keys true)}]))
       (.catch #(rf/dispatch [:machine/dispatch
                              {:type :stats/error
                               :kind kind
                               :request-id request-id
                               :message (.-message %)}])))))

(rf/reg-fx
 :pgmcp/fetch-query
 (fn [{:keys [request request-id token]}]
   (-> (fetch-json "/api/query"
                   {:method "POST"
                    :headers {"content-type" "application/json"}
                    :body (.stringify js/JSON (clj->js request))}
                   token)
       (.then #(rf/dispatch [:machine/dispatch
                             {:type :query/loaded
                              :request-id request-id
                              :payload (js->clj % :keywordize-keys true)}]))
       (.catch #(rf/dispatch [:machine/dispatch
                              {:type :query/error
                               :request-id request-id
                               :message (.-message %)}])))))

(rf/reg-fx
 :pgmcp/fetch-mandates
 (fn [{:keys [request request-id token]}]
   (let [params (url-params request)]
     (-> (fetch-json (str "/api/mandates?" (.toString params)) nil token)
         (.then #(rf/dispatch [:machine/dispatch
                               {:type :mandates/loaded
                                :request-id request-id
                                :payload (js->clj % :keywordize-keys true)}]))
         (.catch #(rf/dispatch [:machine/dispatch
                                {:type :mandates/error
                                :request-id request-id
                                 :message (.-message %)}]))))))

(rf/reg-fx
 :pgmcp/fetch-work
 (fn [{:keys [request request-id token]}]
   (let [params (url-params request)]
     (-> (fetch-json (str "/api/work_items?" (.toString params)) nil token)
         (.then #(rf/dispatch [:machine/dispatch
                               {:type :work/loaded
                                :request-id request-id
                                :payload (js->clj % :keywordize-keys true)}]))
         (.catch #(rf/dispatch [:machine/dispatch
                                {:type :work/error
                                 :request-id request-id
                                 :message (.-message %)}]))))))

(rf/reg-fx
 :pgmcp/fetch-resources
 (fn [{:keys [request-id token]}]
   (-> (fetch-json "/api/resources" nil token)
       (.then #(rf/dispatch [:machine/dispatch
                             {:type :resources/loaded
                              :request-id request-id
                              :payload (js->clj % :keywordize-keys true)}]))
       (.catch #(rf/dispatch [:machine/dispatch
                              {:type :resources/error
                               :request-id request-id
                               :message (.-message %)}])))))

(rf/reg-fx
 :pgmcp/fetch-panel
 (fn [{:keys [panel url request-id token]}]
   (-> (fetch-json url nil token)
       (.then #(rf/dispatch [:machine/dispatch
                             {:type :panel/loaded
                              :panel panel
                              :request-id request-id
                              :payload (js->clj % :keywordize-keys true)}]))
       (.catch #(rf/dispatch [:machine/dispatch
                              {:type :panel/error
                               :panel panel
                               :request-id request-id
                               :message (.-message %)}])))))

(rf/reg-fx
 :pgmcp/ws-connect
 (fn [subscription]
   (when-let [current @socket]
     (.close current))
   (when-not (str/blank? (or (:token subscription) ""))
     (.setItem (.-localStorage js/window) "pgmcp.webui.token" (:token subscription)))
   (let [ws (js/WebSocket. (websocket-url subscription))]
     (reset! socket ws)
     (.addEventListener
      ws
      "open"
      (fn []
        (when (= ws @socket)
          (rf/dispatch [:machine/dispatch {:type :ws/open}])
          (send-hello! ws subscription))))
     (.addEventListener
      ws
      "close"
      (fn []
        (when (= ws @socket)
          (rf/dispatch [:machine/dispatch {:type :ws/closed}]))))
     (.addEventListener
      ws
      "error"
      (fn []
        (when (= ws @socket)
          (rf/dispatch [:machine/dispatch {:type :ws/error}]))))
     (.addEventListener
      ws
      "message"
      (fn [message]
        (when (= ws @socket)
          (try
            (rf/dispatch [:machine/dispatch
                          {:type :ws/frame
                           :frame (js->clj (.parse js/JSON (.-data message))
                                           :keywordize-keys true)}])
            (catch :default _
              (rf/dispatch [:machine/dispatch {:type :ws/error}])))))))))

(rf/reg-fx
 :pgmcp/ws-disconnect
 (fn [_]
   (when-let [current @socket]
     (reset! socket nil)
     (.close current))))

(rf/reg-fx
 :pgmcp/ws-sync-subscription
 (fn [subscription]
   (send-hello! @socket subscription)))

(defn remembered-token []
  (or (.getItem (.-localStorage js/window) "pgmcp.webui.token") ""))

(defn remembered-theme []
  (if (= "light" (.getItem (.-localStorage js/window) "pgmcp.webui.theme"))
    :light
    :dark))
