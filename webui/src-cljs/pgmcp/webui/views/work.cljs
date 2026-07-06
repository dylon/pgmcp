(ns pgmcp.webui.views.work
  (:require [clojure.string :as str]
            [pgmcp.webui.domain :as domain]
            [pgmcp.webui.schema :as schema]
            [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.editor :as editor]
            [pgmcp.webui.views.markdown :as markdown]
            [pgmcp.webui.views.panel :as panel]
            [pgmcp.webui.views.widgets :as w]
            [re-com.core :as rc]
            [re-frame.core :as rf]
            [reagent.core :as r]))

(defn set-field! [field value]
  (rf/dispatch [:machine/dispatch {:type :work/set-field
                                    :field field
                                    :value value}]))

(defn work-form []
  (let [{:keys [view assignee limit plan-public-id kind status project]} @(rf/subscribe [:work/form])
        pending? @(rf/subscribe [:work/pending?])
        tree-mode @(rf/subscribe [:panel/ui-param :work :tree false])]
    [:form.querybar.workbar
     {:on-submit (fn [event]
                   (.preventDefault event)
                   (when-not pending?
                     (rf/dispatch [:machine/dispatch {:type :work/load}])))}
     [rc/single-dropdown
      :class "work-view"
      :choices schema/work-view-choices
      :model view
      :on-change #(set-field! :view %)
      :width "132px"]
     [rc/single-dropdown
      :class "work-view"
      :choices schema/work-kind-choices
      :model (or kind "")
      :on-change #(set-field! :kind %)
      :width "120px"]
     [rc/single-dropdown
      :class "work-view"
      :choices schema/work-status-choices
      :model (or status "")
      :on-change #(set-field! :status %)
      :width "130px"]
     [rc/input-text
      :class "mandate-project"
      :model (or project "")
      :placeholder "project"
      :change-on-blur? false
      :on-change #(set-field! :project %)]
     [rc/input-text
      :class "work-assignee"
      :model assignee
      :placeholder "assignee"
      :change-on-blur? false
      :on-change #(set-field! :assignee %)]
     [rc/input-text
      :class "work-plan"
      :model plan-public-id
      :placeholder "plan id"
      :change-on-blur? false
      :on-change #(set-field! :plan-public-id %)]
     [rc/input-text
      :class "query-limit"
      :model (str limit)
      :placeholder "limit"
      :width "82px"
      :change-on-blur? false
      :validation-regex #"^\d{0,3}$"
      :attr {:aria-label "limit"
             :inputMode "numeric"
             :pattern "[0-9]*"}
      :on-change #(set-field! :limit %)]
     [rc/button
      :label (if pending? "Loading" "Load")
      :disabled? pending?
      :attr {:type "submit"}]
     [rc/checkbox
      :model tree-mode
      :label "tree"
      :on-change #(rf/dispatch [:ui/set-panel-param :work :tree %])]]))

;; Operator-plausible transition targets. The backend transition matrix is the
;; authority — an illegal transition returns 403 (shown inline). `verified` /
;; `rejected` are deliberately absent: they are Gatekeeper/CI-only and would
;; always be refused for an operator.
(def transition-choices
  (mapv (fn [s] {:id s :label s})
        ["pending" "ready" "in_progress" "blocked" "claimed_done" "cancelled"]))

(def severity-choices
  (mapv (fn [s] {:id s :label s}) ["critical" "high" "medium" "low"]))

(defn reload-work []
  [:machine/dispatch {:type :work/load}])

;; A bug must record severity + reproduction (POST /triage) before it can be
;; confirmed (POST /confirm) — the backend enforces this, so surface the form.
(defn bug-triage-form [{:keys [public-id]}]
  (let [sev @(rf/subscribe [:panel/ui-param public-id :severity "medium"])
        repro @(rf/subscribe [:panel/ui-param public-id :repro ""])
        act @(rf/subscribe [:action/status (str "wi-triage:" public-id)])]
    [:div.row-actions
     [rc/single-dropdown
      :choices severity-choices
      :model sev
      :width "110px"
      :on-change #(rf/dispatch [:ui/set-panel-param public-id :severity %])]
     [rc/input-text
      :model repro
      :placeholder "reproduction steps (required to confirm)"
      :width "280px"
      :change-on-blur? false
      :on-change #(rf/dispatch [:ui/set-panel-param public-id :repro %])]
     [ui/toolbar-button
      {:label "Triage"
       :disabled? (or (= :pending act) (str/blank? repro))
       :on-click #(rf/dispatch [:action/submit (str "wi-triage:" public-id)
                                {:method "POST"
                                 :url (str "/api/work_items/" public-id "/triage")
                                 :body {:severity sev :reproduction_steps repro}
                                 :on-success (reload-work)}])}]
     (cond
       (map? act) [:span.editor-err (:error act)]
       (= :done act) [:span.editor-ok "triaged"])]))

(defn work-actions [{:keys [public-id kind status]}]
  (let [to-status @(rf/subscribe [:panel/ui-param public-id :to-status "in_progress"])
        editing? @(rf/subscribe [:panel/ui-param public-id :editing? false])
        act-status @(rf/subscribe [:action/status (str "wi:" public-id)])]
    [:div.row-actions
     [rc/single-dropdown
      :choices transition-choices
      :model to-status
      :width "148px"
      :on-change #(rf/dispatch [:ui/set-panel-param public-id :to-status %])]
     [ui/toolbar-button
      {:label "Set status"
       :disabled? (= :pending act-status)
       :on-click #(rf/dispatch [:action/submit (str "wi:" public-id)
                                {:method "POST"
                                 :url (str "/api/work_items/" public-id "/transition")
                                 :body {:to_status to-status}
                                 :on-success (reload-work)}])}]
     (when (and (= kind "bug") (= status "triage"))
       [ui/toolbar-button
        {:label "Confirm"
         :on-click #(rf/dispatch [:action/submit (str "wi:" public-id)
                                  {:method "POST"
                                   :url (str "/api/work_items/" public-id "/confirm")
                                   :on-success (reload-work)}])}])
     [ui/toolbar-button
      {:label (if editing? "Close editor" "Edit body")
       :on-click #(rf/dispatch [:ui/set-panel-param public-id :editing? (not editing?)])}]
     [ui/toolbar-button
      {:label "Detail"
       :on-click #(rf/dispatch [:ui/set-panel-param :work :detail public-id])}]
     (cond
       (map? act-status) [:span.editor-err (:error act-status)]
       (= :done act-status) [:span.editor-ok "updated"])]))

(defn work-row [{:keys [public-id kind status title body priority claimed-percent
                        assignee claimed-by due-at severity] :as item}]
  (let [editing? @(rf/subscribe [:panel/ui-param public-id :editing? false])]
    [:div.work-row
     [ui/meta-row public-id kind status
      (str "P" priority)
      claimed-percent
      severity
      (when-not (str/blank? assignee) (str "owner " assignee))
      (when-not (str/blank? claimed-by) (str "claim " claimed-by))
      (when-not (str/blank? due-at) (str "due " due-at))]
     [:div.work-title title]
     (when-not (str/blank? body)
       [:div.snippet (ui/preview-text body)])
     [work-actions item]
     (when (and (= kind "bug") (= status "triage"))
       [bug-triage-form item])
     (when editing?
       [editor/editor {:id (str "wi-body:" public-id)
                       :text (or body "")
                       :uri (str "inmemory://" public-id ".md")
                       :save-url (str "/api/work_items/" public-id)
                       :save-method "PATCH"
                       :on-cancel #(rf/dispatch [:ui/set-panel-param public-id :editing? false])}])]))

(defn work-list []
  (let [payload @(rf/subscribe [:work/payload])
        state @(rf/subscribe [:control/work])
        pending? @(rf/subscribe [:work/pending?])
        rows @(rf/subscribe [:work/items])]
    [:div.results
     [ui/summary-row [(name state)
                      (when pending? "loading")
                      (when (:view payload) (:view payload))
                      (when (some? (:count payload))
                        (str (:count payload) " rows"))]]
     (cond
       (nil? payload)
       [ui/empty-box (if pending?
                       "Loading work items."
                       "No work view loaded.")]

       (:error payload)
       [ui/error-box (:error payload)]

       (seq rows)
       (for [[idx row] (map-indexed vector rows)]
         ^{:key (str (:public-id row) ":" idx)}
         [work-row row])

       :else
       [ui/empty-box "No work items."])]))

(defn work-detail [public-id]
  (r/with-let [_ (panel/load! :work-detail (str "/api/work_items/" (js/encodeURIComponent public-id)))]
    (let [payload @(rf/subscribe [:panel/payload :work-detail])
          item (:item payload)]
      [:div.exp-detail
       [:div.toolbar
        [ui/toolbar-button {:label "Close detail"
                            :on-click #(rf/dispatch [:ui/set-panel-param :work :detail nil])}]]
       (cond
         (nil? payload) [ui/empty-box "Loading…"]
         (:error payload) [ui/error-box (:error payload)]
         :else
         [:div
          [w/sections (domain/normalized-work-detail payload)]
          (when-not (str/blank? (:body item))
            [:div
             [:div.new-mandate-title "Body"]
             [markdown/markdown-view (str "wi-detail:" public-id) (:body item)]])])])))

(defn work-tree [root]
  (r/with-let [_ (when-not (str/blank? (str root))
                   (panel/load! :work-tree
                                (str "/api/work_items/tree?root=" (js/encodeURIComponent root))))]
    (let [payload @(rf/subscribe [:panel/payload :work-tree])
          nodes (domain/normalized-work-tree payload)]
      (cond
        (str/blank? (str root))
        [ui/empty-box "Enter a plan/epic id in 'plan id', then switch to Tree."]

        (nil? payload) [ui/empty-box "Loading tree…"]
        (:error payload) [ui/error-box (:error payload)]

        (seq nodes)
        [:div.results
         (for [[idx n] (map-indexed vector nodes)]
           ^{:key (str (:public-id n) ":" idx)}
           [:div.tree-row {:style {:padding-left (str (+ 4 (* 18 (:depth n))) "px")}}
            [ui/meta-row (:public-id n) (:kind n) (:status n)]
            [:span.work-title (:title n)]
            [ui/toolbar-button {:label "Detail"
                                :on-click #(rf/dispatch [:ui/set-panel-param :work :detail (:public-id n)])}]])]

        :else [ui/empty-box "No tree nodes."]))))

(defn work-page []
  (let [{:keys [plan-public-id]} @(rf/subscribe [:work/form])
        tree-mode @(rf/subscribe [:panel/ui-param :work :tree false])
        detail @(rf/subscribe [:panel/ui-param :work :detail nil])]
    [ui/page
     "work-view"
     [work-form]
     (if tree-mode
       ^{:key (str "tree-" plan-public-id)}
       [work-tree plan-public-id]
       [work-list])
     (when-not (str/blank? (str detail))
       ^{:key detail}
       [work-detail detail])]))
