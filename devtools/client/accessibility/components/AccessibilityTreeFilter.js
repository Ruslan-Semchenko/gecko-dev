/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
"use strict";

// React
const {
  createFactory,
  Component,
} = require("resource://devtools/client/shared/vendor/react.mjs");
const {
  div,
  hr,
  span,
} = require("resource://devtools/client/shared/vendor/react-dom-factories.js");
const PropTypes = require("resource://devtools/client/shared/vendor/react-prop-types.mjs");
const {
  L10N,
} = require("resource://devtools/client/accessibility/utils/l10n.js");

loader.lazyGetter(this, "MenuButton", function () {
  return createFactory(
    require("resource://devtools/client/shared/components/menu/MenuButton.js")
  );
});
loader.lazyGetter(this, "MenuItem", function () {
  return createFactory(
    require("resource://devtools/client/shared/components/menu/MenuItem.js")
  );
});
loader.lazyGetter(this, "MenuList", function () {
  return createFactory(
    require("resource://devtools/client/shared/components/menu/MenuList.js")
  );
});

const actions = require("resource://devtools/client/accessibility/actions/audit.js");

const {
  connect,
} = require("resource://devtools/client/shared/vendor/react-redux.js");
const {
  FILTERS,
} = require("resource://devtools/client/accessibility/constants.js");

const FILTER_LABELS = {
  [FILTERS.NONE]: "accessibility.filter.none",
  [FILTERS.ALL]: "accessibility.filter.all2",
  [FILTERS.CONTRAST]: "accessibility.filter.contrast",
  [FILTERS.KEYBOARD]: "accessibility.filter.keyboard",
  [FILTERS.TEXT_LABEL]: "accessibility.filter.textLabel",
};

class AccessibilityTreeFilter extends Component {
  static get propTypes() {
    return {
      auditing: PropTypes.array.isRequired,
      filters: PropTypes.object.isRequired,
      dispatch: PropTypes.func.isRequired,
      describedby: PropTypes.string,
      toolboxDoc: PropTypes.object.isRequired,
      audit: PropTypes.func.isRequired,
    };
  }

  async toggleFilter(filterKey) {
    const { audit: auditFunc, dispatch, filters } = this.props;

    if (filterKey !== FILTERS.NONE && !filters[filterKey]) {
      Glean.devtoolsAccessibility.auditActivated[filterKey].add(1);

      dispatch(actions.auditing(filterKey));
      await dispatch(actions.audit(auditFunc, filterKey));
    }

    // We wait to dispatch filter toggle until the tree is ready to be filtered
    // right after the audit. This is to make sure that we render an empty tree
    // (filtered) while the audit is running.
    dispatch(actions.filterToggle(filterKey));
  }

  onClick(filterKey) {
    this.toggleFilter(filterKey);
  }

  render() {
    const { auditing, filters, describedby, toolboxDoc } = this.props;
    const toolbarLabelID = "accessibility-tree-filters-label";
    const filterNoneChecked = !Object.values(filters).includes(true);
    const items = [
      MenuItem({
        key: FILTERS.NONE,
        checked: filterNoneChecked,
        className: `filter ${FILTERS.NONE}`,
        label: L10N.getStr(FILTER_LABELS[FILTERS.NONE]),
        onClick: this.onClick.bind(this, FILTERS.NONE),
        disabled: !!auditing.length,
      }),
      hr({ key: "hr-1" }),
    ];

    const { [FILTERS.ALL]: filterAllChecked, ...filtersWithoutAll } = filters;
    items.push(
      MenuItem({
        key: FILTERS.ALL,
        checked: filterAllChecked,
        className: `filter ${FILTERS.ALL}`,
        label: L10N.getStr(FILTER_LABELS[FILTERS.ALL]),
        onClick: this.onClick.bind(this, FILTERS.ALL),
        disabled: !!auditing.length,
      }),
      hr({ key: "hr-2" }),
      Object.entries(filtersWithoutAll).map(([filterKey, active]) =>
        MenuItem({
          key: filterKey,
          checked: active,
          className: `filter ${filterKey}`,
          label: L10N.getStr(FILTER_LABELS[filterKey]),
          onClick: this.onClick.bind(this, filterKey),
          disabled: !!auditing.length,
        })
      )
    );

    let label;
    if (filterNoneChecked) {
      label = L10N.getStr(FILTER_LABELS[FILTERS.NONE]);
    } else if (filterAllChecked) {
      label = L10N.getStr(FILTER_LABELS[FILTERS.ALL]);
    } else {
      label = Object.keys(filtersWithoutAll)
        .filter(filterKey => filtersWithoutAll[filterKey])
        .map(filterKey => L10N.getStr(FILTER_LABELS[filterKey]))
        .join(", ");
    }

    return div(
      {
        role: "group",
        className: "accessibility-tree-filters",
        "aria-labelledby": toolbarLabelID,
        "aria-describedby": describedby,
      },
      span(
        { id: toolbarLabelID, role: "presentation" },
        L10N.getStr("accessibility.tree.filters")
      ),
      MenuButton(
        {
          menuId: "accessibility-tree-filters-menu",
          toolboxDoc,
          className: `devtools-button badge toolbar-menu-button filters`,
          label,
        },
        MenuList({}, items)
      )
    );
  }
}

const mapStateToProps = ({ audit: { filters, auditing } }) => {
  return { filters, auditing };
};

// Exports from this module
module.exports = connect(mapStateToProps)(AccessibilityTreeFilter);
