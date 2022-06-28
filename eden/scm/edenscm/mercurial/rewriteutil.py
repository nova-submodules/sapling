# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# rewriteutil.py - utility functions for rewriting changesets
#
# Copyright 2017 Octobus <contact@octobus.net>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

from __future__ import absolute_import

from . import error, node
from .i18n import _


def precheck(repo, revs, action="rewrite"):
    """check if revs can be rewritten
    action is used to control the error message.

    Make sure this function is called after taking the lock.
    """
    if node.nullrev in revs:
        msg = _("cannot %s null changeset") % action
        hint = _("no changeset checked out")
        raise error.Abort(msg, hint=hint)

    publicrevs = repo.revs("%ld and public()", revs)
    if len(repo[None].parents()) > 1:
        raise error.Abort(_("cannot %s while merging") % action)

    if publicrevs:
        msg = _("cannot %s public changesets") % action
        hint = _("see 'hg help phases' for details")
        raise error.Abort(msg, hint=hint)
