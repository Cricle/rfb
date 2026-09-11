import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import * as validation from '../validation.js';
import { ValidationError } from '../errors.js';

describe('validation', () => {
  it('fsPath rejects empty and escaping paths', () => {
    assert.throws(() => validation.fsPath(''), ValidationError);
    assert.throws(() => validation.fsPath('../etc/passwd'), ValidationError);
    assert.throws(() => validation.fsPath('a\\b'), ValidationError);
    assert.throws(() => validation.fsPath('/etc/passwd'), ValidationError);
  });
  it('fsPath accepts relative and /workspace paths', () => {
    validation.fsPath('.');
    validation.fsPath('notes.txt');
    validation.fsPath('/workspace');
    validation.fsPath('/workspace/sub');
  });
  it('filePath rejects drive prefixes and escaping', () => {
    assert.throws(() => validation.filePath('C:\\x'), ValidationError);
    assert.throws(() => validation.filePath('../x'), ValidationError);
    validation.filePath('relative.txt');
    validation.filePath('/workspace/f.txt');
  });
  it('pattern rejects empty, NUL, oversize', () => {
    assert.throws(() => validation.pattern(''), ValidationError);
    assert.throws(() => validation.pattern('a\0b'), ValidationError);
    validation.pattern('*.rs');
  });
  it('limit enforces 1..max', () => {
    assert.throws(() => validation.limit(0, 100), ValidationError);
    assert.throws(() => validation.limit(101, 100), ValidationError);
    validation.limit(1, 100);
  });
  it('argv rejects empty and non-string', () => {
    assert.throws(() => validation.argv([]), ValidationError);
    assert.throws(() => validation.argv([1] as unknown as string[]), ValidationError);
    validation.argv(['echo', 'hi']);
  });
  it('sandboxId enforces [A-Za-z0-9_-] ≤ 128', () => {
    assert.throws(() => validation.sandboxId(''), ValidationError);
    assert.throws(() => validation.sandboxId('a b'), ValidationError);
    validation.sandboxId('sb-123_ABC');
  });
});
