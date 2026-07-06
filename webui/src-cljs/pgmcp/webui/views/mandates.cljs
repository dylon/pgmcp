(ns pgmcp.webui.views.mandates
  (:require [clojure.string :as str]
            [pgmcp.webui.schema :as schema]
            [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.editor :as editor]
            [pgmcp.webui.views.markdown :as markdown]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(defn set-field! [field value]
  (rf/dispatch [:machine/dispatch {:type :mandates/set-field
                                    :field field
                                    :value value}]))

(defn mandates-form []
  (let [{:keys [scope project]} @(rf/subscribe [:mandates/form])
        pending? @(rf/subscribe [:mandates/pending?])]
    [:form.querybar
     {:on-submit (fn [event]
                   (.preventDefault event)
                   (when-not pending?
                     (rf/dispatch [:machine/dispatch {:type :mandates/load}])))}
     [rc/single-dropdown
      :choices schema/mandate-scope-choices
      :model scope
      :on-change #(set-field! :scope %)
      :width "132px"]
     [rc/input-text
      :class "mandate-project"
      :model project
      :placeholder "project"
      :change-on-blur? false
      :on-change #(set-field! :project %)]
     [rc/button
      :label (if pending? "Loading" "Load")
      :disabled? pending?
      :attr {:type "submit"}]]))

(def durable-scope-choices
  (mapv (fn [s] {:id s :label s}) ["global" "project" "workspace"]))

(def polarity-choices
  (mapv (fn [s] {:id s :label s})
        ["always" "never" "prefer" "avoid" "remember" "from_now_on"
         "correction" "permission" "constraint" "mandate" "process_rule" "project_rule"]))

(defn set-new! [key value]
  (rf/dispatch [:ui/set-panel-param :new-mandate key value]))

(defn new-mandate-form []
  (let [scope @(rf/subscribe [:panel/ui-param :new-mandate :scope "global"])
        polarity @(rf/subscribe [:panel/ui-param :new-mandate :polarity "always"])
        imperative @(rf/subscribe [:panel/ui-param :new-mandate :imperative ""])
        target @(rf/subscribe [:panel/ui-param :new-mandate :target ""])
        project @(rf/subscribe [:panel/ui-param :new-mandate :project ""])
        status @(rf/subscribe [:action/status :new-mandate])]
    [:div.new-mandate
     [:div.new-mandate-title "New durable mandate"]
     [:div.chips-row
      [rc/single-dropdown :choices durable-scope-choices :model scope :width "120px"
       :on-change #(set-new! :scope %)]
      [rc/single-dropdown :choices polarity-choices :model polarity :width "150px"
       :on-change #(set-new! :polarity %)]
      (when (= scope "project")
        [rc/input-text :class "mandate-project" :model project :placeholder "project"
         :change-on-blur? false :on-change #(set-new! :project %)])]
     [rc/input-text :class "mandate-imperative" :model imperative
      :placeholder "imperative (the rule)" :width "100%" :change-on-blur? false
      :on-change #(set-new! :imperative %)]
     [rc/input-text :class "mandate-target" :model target
      :placeholder "target (optional, e.g. a file glob)" :width "100%" :change-on-blur? false
      :on-change #(set-new! :target %)]
     [:div.chips-row
      [ui/toolbar-button
       {:label "Create"
        :disabled? (or (= :pending status) (str/blank? imperative))
        :on-click #(rf/dispatch [:action/submit :new-mandate
                                 {:method "POST"
                                  :url "/api/mandates/durable"
                                  :body {:scope scope
                                         :polarity polarity
                                         :imperative imperative
                                         :target target
                                         :project (when (= scope "project") project)}
                                  :on-success [:machine/dispatch {:type :mandates/load}]}])}]
      (cond
        (map? status) [:span.editor-err (:error status)]
        (= :done status) [:span.editor-ok "created"])]]))

(defn mandate-row [id source]
  [:div.mandate-row
   [ui/meta-row (:scope source) (:kind source) (:path source)
    (case (:row-kind source)
      :project-override "project override"
      :skipped "skipped"
      nil)]
   (if (= :skipped (:row-kind source))
     [:pre (or (:text source) "")]
     [markdown/markdown-view id (or (:text source) "")])])

(defn durable-mandate-row [m]
  (let [id (:id m)
        key (str "dm:" id)
        editing? @(rf/subscribe [:panel/ui-param key :editing? false])
        act @(rf/subscribe [:action/status key])]
    [:div.mandate-row
     [ui/meta-row (:scope m) (:polarity m) (str "#" id)
      (when (:retired_at m) "retired")]
     [markdown/markdown-view (str "dm-view:" id) (or (:imperative m) "")]
     (when-not (str/blank? (:target m))
       [:div.snippet (str "target: " (:target m))])
     [:div.row-actions
      [ui/toolbar-button
       {:label (if editing? "Close editor" "Edit")
        :on-click #(rf/dispatch [:ui/set-panel-param key :editing? (not editing?)])}]
      (when-not (:retired_at m)
        [ui/toolbar-button
         {:label "Retire"
          :disabled? (= :pending act)
          :on-click #(rf/dispatch [:action/submit key
                                   {:method "POST"
                                    :url (str "/api/mandates/durable/" id "/retire")
                                    :on-success [:machine/dispatch {:type :mandates/load}]}])}])
      (cond
        (map? act) [:span.editor-err (:error act)]
        (= :done act) [:span.editor-ok "ok"])]
     (when editing?
       [editor/editor {:id (str "dm-edit:" id)
                       :text (or (:imperative m) "")
                       :uri (str "inmemory://mandate-" id ".md")
                       :save-url (str "/api/mandates/durable/" id)
                       :save-method "PATCH"
                       :on-cancel #(rf/dispatch [:ui/set-panel-param key :editing? false])}])]))

(defn durable-mandates-block [payload]
  (when-let [dms (seq (:durable_mandates payload))]
    [:div.durable-mandates
     [:div.new-mandate-title (str "Durable mandates (" (count dms) ")")]
     (for [[idx m] (map-indexed vector dms)]
       ^{:key (str "dm:" (:id m) ":" idx)}
       [durable-mandate-row m])]))

(defn mandate-list []
  (let [payload @(rf/subscribe [:mandates/payload])
        state @(rf/subscribe [:control/mandates])
        pending? @(rf/subscribe [:mandates/pending?])
        sources @(rf/subscribe [:mandates/sources])]
    [:div.results
     [ui/summary-row [(name state)
                      (when pending? "loading")]]
     (cond
       (nil? payload)
       [ui/empty-box (if pending?
                       "Loading mandates."
                       "No mandates loaded.")]

       (:error payload)
       [ui/error-box (:error payload)]

       :else
       [:<>
        [durable-mandates-block payload]
        (cond
          (seq sources)
          (for [[idx source] (map-indexed vector sources)]
            ^{:key (str (:path source) ":" idx)}
            [mandate-row (str "mandate:" (:path source) ":" idx) source])

          (seq (:durable_mandates payload)) nil

          :else
          [ui/empty-box "No mandate sources."])])]))

(defn mandates-page []
  [ui/page
   "mandates-view"
   [mandates-form]
   [new-mandate-form]
   [mandate-list]])
