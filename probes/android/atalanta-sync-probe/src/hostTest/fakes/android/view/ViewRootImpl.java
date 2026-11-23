// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package android.view;

/**
 * Host-check fake: AOSP's hidden ViewRootImpl and its {@code final class
 * ViewRootHandler extends Handler}, the {@code AttachInfo.mHandler} that
 * {@code View.postDelayed} posts to.
 */
public final class ViewRootImpl {
    public final ViewRootHandler mHandler = new ViewRootHandler();

    public final class ViewRootHandler {}
}
