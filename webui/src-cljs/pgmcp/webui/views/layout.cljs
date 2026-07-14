(ns pgmcp.webui.views.layout
  "Reusable master-detail layout. A list region and an optional detail region
  sit side-by-side (split pane) on wide viewports and collapse to a stacked
  replace-with-Back on narrow ones. Layout only — the caller owns the list and
  detail hiccup plus the open/close state; nothing here touches the statechart."
  (:require [re-com.core :as rc]))

(defn master-detail
  "opts: {:list <hiccup> :detail <hiccup|nil> :title <str> :on-close <fn>}.

  With :detail nil the list fills the width. With :detail present a split pane
  shows the list on the left and the detail (sticky header with title + Close)
  on the right, so selecting a row never scrolls the detail off-screen. Below
  900px CSS collapses this to a single column where the detail replaces the list
  and a ‹ Back control returns to it."
  [{:keys [list detail title on-close]}]
  (if (nil? detail)
    [:div.master-detail
     [:div.md-list list]]
    [:div.master-detail.md-open
     [:div.md-list list]
     [:div.md-detail
      [:div.md-detail-head
       [rc/button
        :class "md-back"
        :label "‹ Back"
        :attr {:title "Back to list"}
        :on-click on-close]
       [:span.md-title (str title)]
       [rc/button
        :class "md-close"
        :label "✕"
        :attr {:title "Close detail"}
        :on-click on-close]]
      [:div.md-detail-body detail]]]))
