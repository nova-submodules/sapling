/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {OSSCommitMessageFieldsUtils} from '../CommitMessageFields';

describe('InternalCommitInfoFields', () => {
  it('parses messages correctly', () => {
    const parsed = OSSCommitMessageFieldsUtils.parseCommitMessageFields(
      'my title',
      `My description!
another line
`,
    );

    expect(parsed.title).toEqual('my title');
    expect(parsed.description).toEqual('My description!\nanother line\n');
  });

  it('converts to string properly', () => {
    expect(
      OSSCommitMessageFieldsUtils.commitMessageFieldsToString({
        title: 'my title',
        description: 'my summary\nline 2',
      }),
    ).toEqual(
      `my title
my summary
line 2`,
    );
  });
});
